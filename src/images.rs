//! Downloading, caching and shrinking picture attachments off the UI thread.

use anyhow::{Context, Result};
use image::{DynamicImage, ImageDecoder, ImageFormat, ImageReader};
use ratatui_image::sliced::SlicedProtocol;
use std::io::Cursor;
use std::time::{Duration, Instant};
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::Semaphore;
use tokio::sync::mpsc::UnboundedSender;

use crate::api::{Api, encode};
use crate::model::Attachment;
use crate::app::AppEvent;
use crate::config::cache_dir;

/// Longest edge of the copy kept on disk. Big enough for any terminal cell
/// box we draw into, small enough that the cache stays modest.
const CACHE_EDGE: u32 = 900;

pub enum ImageState {
    Loading,
    Ready(SlicedProtocol),
    /// A GIF: one picture per frame, how long each shows, and when it
    /// started playing.
    Animated {
        frames: Vec<SlicedProtocol>,
        delays: Vec<Duration>,
        started: Instant,
    },
    Failed,
}

impl ImageState {
    /// The picture to draw right now: the only one, or a GIF's current
    /// frame.
    pub fn current(&self) -> Option<&SlicedProtocol> {
        match self {
            ImageState::Ready(picture) => Some(picture),
            ImageState::Animated {
                frames,
                delays,
                started,
            } => {
                let cycle: Duration = delays.iter().sum();
                if cycle.is_zero() {
                    return frames.first();
                }
                // Where in the loop we are.
                let mut into = Duration::from_nanos(
                    (started.elapsed().as_nanos() % cycle.as_nanos()) as u64,
                );
                for (frame, delay) in frames.iter().zip(delays) {
                    if into < *delay {
                        return Some(frame);
                    }
                    into -= *delay;
                }
                frames.last()
            }
            _ => None,
        }
    }

    /// For a GIF, the shortest time any frame shows, which is how often
    /// the screen needs redrawing to play it.
    pub fn frame_time(&self) -> Option<Duration> {
        match self {
            ImageState::Animated { delays, .. } => delays.iter().min().copied(),
            _ => None,
        }
    }
}

/// What a fetch produced: one picture, or a GIF's frames with how long
/// each one shows.
pub enum Loaded {
    Still(DynamicImage),
    Frames(Vec<(DynamicImage, Duration)>),
}

/// The most frames of a GIF kept. Longer ones keep every second or third
/// frame, with the timing adjusted so they play at the same speed.
const MAX_FRAMES: usize = 40;
/// GIFs larger than this are shown as a still, to keep memory in check.
const MAX_GIF_BYTES: usize = 12 * 1024 * 1024;

/// Decode a GIF into frames scaled to fit the box. `Ok(None)` if it turns
/// out to have a single frame.
pub fn decode_gif(bytes: &[u8], box_px: (u32, u32)) -> Result<Option<Vec<(DynamicImage, Duration)>>> {
    use image::AnimationDecoder;
    let decoder = image::codecs::gif::GifDecoder::new(Cursor::new(bytes))?;
    let frames = decoder.into_frames().collect_frames()?;
    if frames.len() < 2 {
        return Ok(None);
    }
    let step = frames.len().div_ceil(MAX_FRAMES);
    let mut kept: Vec<(DynamicImage, Duration)> = Vec::new();
    for (i, frame) in frames.into_iter().enumerate() {
        let (numer, denom) = frame.delay().numer_denom_ms();
        let mut delay = Duration::from_millis(u64::from(numer) / u64::from(denom.max(1)));
        // GIFs that ask for (nearly) no delay are played at ten frames a
        // second by everything else, so do the same.
        if delay < Duration::from_millis(20) {
            delay = Duration::from_millis(100);
        }
        if i % step == 0 {
            let picture = DynamicImage::ImageRgba8(frame.into_buffer());
            kept.push((shrink(picture, box_px.0, box_px.1), delay));
        } else if let Some((_, shown)) = kept.last_mut() {
            // A skipped frame's time goes to the one shown in its place.
            *shown += delay;
        }
    }
    Ok(Some(kept))
}

fn gif_cache_path(guid: &str) -> PathBuf {
    cache_dir().join("gifs").join(encode(guid))
}

/// A GIF's frames, from the cached file or freshly downloaded.
async fn load_gif(
    api: &Api,
    attachment: &Attachment,
    box_px: (u32, u32),
) -> Result<Option<Vec<(DynamicImage, Duration)>>> {
    let path = gif_cache_path(&attachment.guid);
    let bytes = match tokio::fs::read(&path).await {
        Ok(bytes) => bytes,
        Err(_) => {
            let bytes = api.attachment(&attachment.guid).await?;
            if let Some(dir) = path.parent() {
                let _ = tokio::fs::create_dir_all(dir).await;
            }
            let _ = tokio::fs::write(&path, &bytes).await;
            bytes
        }
    };
    tokio::task::spawn_blocking(move || decode_gif(&bytes, box_px)).await?
}

fn cache_path(guid: &str) -> PathBuf {
    cache_dir().join("thumbs").join(encode(guid))
}

/// Decode and apply the camera orientation, which phone photos rely on.
pub fn decode(bytes: &[u8]) -> Result<DynamicImage> {
    let reader = ImageReader::new(Cursor::new(bytes)).with_guessed_format()?;
    let mut decoder = reader.into_decoder()?;
    let orientation = decoder.orientation().ok();
    let mut img = DynamicImage::from_decoder(decoder)?;
    if let Some(orientation) = orientation {
        img.apply_orientation(orientation);
    }
    Ok(img)
}

pub fn shrink(img: DynamicImage, max_w: u32, max_h: u32) -> DynamicImage {
    if img.width() <= max_w && img.height() <= max_h {
        img
    } else {
        img.thumbnail(max_w.max(1), max_h.max(1))
    }
}

fn write_cache(guid: &str, img: &DynamicImage) -> Result<()> {
    let path = cache_path(guid);
    let dir = path.parent().context("cache path has no parent")?;
    std::fs::create_dir_all(dir)?;
    let mut bytes = Vec::new();
    if img.color().has_alpha() {
        img.write_to(&mut Cursor::new(&mut bytes), ImageFormat::Png)?;
    } else {
        img.to_rgb8()
            .write_to(&mut Cursor::new(&mut bytes), ImageFormat::Jpeg)?;
    }
    // Write then rename so a half-written file is never read back.
    let tmp = dir.join(format!(".{}.tmp", std::process::id()));
    std::fs::write(&tmp, bytes)?;
    std::fs::rename(&tmp, &path)?;
    Ok(())
}

/// Whether ffmpeg is installed, which video stills depend on.
pub fn can_make_stills() -> bool {
    std::process::Command::new("ffmpeg")
        .arg("-version")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

/// One representative frame of a video, as JPEG bytes.
fn video_still(path: &std::path::Path) -> Result<Vec<u8>> {
    let output = std::process::Command::new("ffmpeg")
        .args(["-v", "error", "-i"])
        .arg(path)
        .args([
            // `thumbnail` picks a frame that is not a black first frame.
            "-vf",
            "thumbnail,scale='min(900,iw)':-2",
            "-frames:v",
            "1",
            "-f",
            "image2pipe",
            "-vcodec",
            "mjpeg",
            "-",
        ])
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .output()
        .context("couldn't run ffmpeg")?;
    if output.stdout.is_empty() {
        anyhow::bail!("ffmpeg produced no frame");
    }
    Ok(output.stdout)
}

async fn load(api: &Api, attachment: &Attachment, box_px: (u32, u32)) -> Result<DynamicImage> {
    let guid = attachment.guid.as_str();
    let path = cache_path(guid);
    if let Ok(bytes) = tokio::fs::read(&path).await {
        let cached = tokio::task::spawn_blocking(move || {
            decode(&bytes).map(|img| shrink(img, box_px.0, box_px.1))
        })
        .await?;
        if let Ok(img) = cached {
            return Ok(img);
        }
    }
    let bytes = if attachment.is_video() {
        // The whole video is needed to pull a frame out of it; it stays in
        // the files cache, so playing it afterwards starts at once.
        let video = crate::files::local_copy(api, attachment, None).await?;
        tokio::task::spawn_blocking(move || video_still(&video)).await??
    } else {
        api.attachment(guid).await?
    };
    let guid = guid.to_string();
    tokio::task::spawn_blocking(move || {
        let img = shrink(decode(&bytes)?, CACHE_EDGE, CACHE_EDGE);
        let _ = write_cache(&guid, &img);
        Ok(shrink(img, box_px.0, box_px.1))
    })
    .await?
}

/// Fetch one picture (or a still of one video) in the background and
/// report back to the app.
pub fn fetch(
    api: Api,
    attachment: Attachment,
    box_px: (u32, u32),
    animate: bool,
    limit: Arc<Semaphore>,
    tx: UnboundedSender<AppEvent>,
) {
    tokio::spawn(async move {
        let _permit = limit.acquire().await;
        let guid = attachment.guid.clone();
        // A GIF plays if the terminal can swap pictures cheaply and it is
        // not enormous; otherwise, and for everything else, one picture.
        let small_enough = attachment
            .total_bytes
            .is_none_or(|bytes| bytes as usize <= MAX_GIF_BYTES);
        if animate && attachment.is_gif() && small_enough
            && let Ok(Some(frames)) = load_gif(&api, &attachment, box_px).await
        {
            let _ = tx.send(AppEvent::Image {
                guid,
                result: Ok(Loaded::Frames(frames)),
            });
            return;
        }
        let result = load(&api, &attachment, box_px).await.map(Loaded::Still).map_err(|e| {
            crate::config::log(&format!("picture {guid} failed: {e:#}"));
            e.to_string()
        });
        let _ = tx.send(AppEvent::Image { guid, result });
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::codecs::gif::GifEncoder;
    use image::{Delay, Frame, Rgba, RgbaImage};

    fn gif(frames: usize, delay_ms: u32) -> Vec<u8> {
        let mut bytes = Vec::new();
        {
            let mut encoder = GifEncoder::new(&mut bytes);
            for i in 0..frames {
                let picture = RgbaImage::from_pixel(8, 6, Rgba([(i * 40) as u8, 90, 200, 255]));
                let delay = Delay::from_numer_denom_ms(delay_ms, 1);
                encoder.encode_frame(Frame::from_parts(picture, 0, 0, delay)).unwrap();
            }
        }
        bytes
    }

    #[test]
    fn reads_frames_and_timing_from_a_gif() {
        let frames = decode_gif(&gif(5, 80), (100, 100)).unwrap().unwrap();
        assert_eq!(frames.len(), 5);
        assert!(frames.iter().all(|(_, delay)| *delay == Duration::from_millis(80)));
        assert_eq!((frames[0].0.width(), frames[0].0.height()), (8, 6));
    }

    #[test]
    fn a_single_frame_gif_is_not_an_animation() {
        assert!(decode_gif(&gif(1, 80), (100, 100)).unwrap().is_none());
    }

    #[test]
    fn long_gifs_are_thinned_but_keep_their_length() {
        let frames = decode_gif(&gif(100, 50), (100, 100)).unwrap().unwrap();
        assert!(frames.len() <= MAX_FRAMES);
        let total: Duration = frames.iter().map(|(_, delay)| *delay).sum();
        assert_eq!(total, Duration::from_millis(100 * 50));
    }

    #[test]
    fn frames_asking_for_no_delay_play_at_a_sane_speed() {
        let frames = decode_gif(&gif(3, 0), (100, 100)).unwrap().unwrap();
        assert!(frames.iter().all(|(_, delay)| *delay == Duration::from_millis(100)));
    }

    #[test]
    fn the_current_frame_follows_the_clock() {
        let picker = ratatui_image::picker::Picker::halfblocks();
        let make = || {
            SlicedProtocol::new(
                &picker,
                DynamicImage::ImageRgba8(RgbaImage::from_pixel(8, 8, Rgba([1, 2, 3, 255]))),
                None,
            )
            .unwrap()
        };
        let state = ImageState::Animated {
            frames: vec![make(), make(), make()],
            delays: vec![Duration::from_millis(100); 3],
            started: Instant::now() - Duration::from_millis(250),
        };
        let ImageState::Animated { frames, .. } = &state else { unreachable!() };
        // 250 ms in, with 100 ms frames, is the third frame.
        assert!(std::ptr::eq(state.current().unwrap(), &frames[2]));
        assert_eq!(state.frame_time(), Some(Duration::from_millis(100)));
    }
}
