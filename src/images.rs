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
const CACHE_EDGE: u32 = 640;
/// JPEG quality for cached copies: plenty for something drawn a few
/// hundred pixels wide, at about a third of the size of the default.
const CACHE_QUALITY: u8 = 72;

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
/// The most memory decoding a GIF may take. A GIF states its own canvas
/// size, and every frame is drawn at that size however little the file
/// holds, so a file of a few hundred bytes can ask for gigabytes. Anyone
/// can send one. This allows a canvas of roughly 3000 by 3000, far beyond
/// any real GIF; bigger ones are refused and shown as a still or not at all.
const MAX_GIF_MEMORY: u64 = 128 * 1024 * 1024;
/// The most memory decoding any one picture may take, for the same
/// reason: a picture's stated size, not the size of its file, is what
/// decides how much is asked for. Room for about 85 megapixels, which is
/// more than phones take; anything bigger is not drawn (it can still be
/// opened or saved).
const MAX_PICTURE_MEMORY: u64 = 256 * 1024 * 1024;
/// The most frames read from one GIF. Past this it simply plays shorter.
const MAX_GIF_FRAMES_READ: usize = 1500;

/// Decode a GIF into frames scaled to fit the box. `Ok(None)` if it turns
/// out to have a single frame.
pub fn decode_gif(bytes: &[u8], box_px: (u32, u32)) -> Result<Option<Vec<(DynamicImage, Duration)>>> {
    use image::AnimationDecoder;
    let mut decoder = image::codecs::gif::GifDecoder::new(Cursor::new(bytes))?;
    let mut limits = image::Limits::default();
    limits.max_alloc = Some(MAX_GIF_MEMORY);
    decoder.set_limits(limits)?;
    // Frames are taken one at a time and shrunk as they come, so only the
    // small copies are ever held. How many there will be is not known
    // until the end, so start by keeping every frame, and each time there
    // are too many, drop every other one and keep half as often from then.
    let mut step = 1;
    let mut read = 0;
    let mut kept: Vec<(DynamicImage, Duration)> = Vec::new();
    for (i, frame) in decoder.into_frames().take(MAX_GIF_FRAMES_READ).enumerate() {
        let frame = frame?;
        read += 1;
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
            if kept.len() > MAX_FRAMES {
                let mut thinned: Vec<(DynamicImage, Duration)> = Vec::with_capacity(kept.len() / 2 + 1);
                for (n, (picture, delay)) in kept.drain(..).enumerate() {
                    if n % 2 == 0 {
                        thinned.push((picture, delay));
                    } else if let Some((_, shown)) = thinned.last_mut() {
                        *shown += delay;
                    }
                }
                kept = thinned;
                step *= 2;
            }
        } else if let Some((_, shown)) = kept.last_mut() {
            // A skipped frame's time goes to the one shown in its place.
            *shown += delay;
        }
    }
    if read < 2 {
        return Ok(None);
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
        Ok(bytes) => {
            crate::storage::touch(&path);
            bytes
        }
        Err(_) => {
            let bytes = api.attachment(&attachment.guid).await?;
            if bytes.len() > MAX_GIF_BYTES {
                // Bigger than the server said it was: a still will do.
                return Ok(None);
            }
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
    // Taking the decoder apart from the reader (which is what gives the
    // orientation) skips the reader's own check that the finished picture
    // fits in memory, so make it here, before anything is asked for.
    let mut limits = image::Limits::default();
    limits.max_alloc = Some(MAX_PICTURE_MEMORY);
    limits.reserve(decoder.total_bytes())?;
    decoder.set_limits(limits)?;
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
    write_cache_in(dir, guid, img)
}

fn write_cache_in(dir: &std::path::Path, guid: &str, img: &DynamicImage) -> Result<()> {
    let path = dir.join(encode(guid));
    std::fs::create_dir_all(dir)?;
    let mut bytes = Vec::new();
    // PNG is many times larger, so it is used only for pictures that
    // really have see-through parts. Plenty of pictures (screenshots
    // especially) carry a transparency channel that is entirely opaque.
    let see_through = img.color().has_alpha() && img.to_rgba8().pixels().any(|p| p.0[3] < 255);
    if see_through {
        img.write_to(&mut Cursor::new(&mut bytes), ImageFormat::Png)?;
    } else {
        let encoder = image::codecs::jpeg::JpegEncoder::new_with_quality(&mut bytes, CACHE_QUALITY);
        img.to_rgb8().write_with_encoder(encoder)?;
    }
    // Write then rename so a half-written file is never read back. The
    // name in between is this picture's own: several are cached at once,
    // and sharing one let a picture land under another's name.
    let tmp = dir.join(format!(".{}.{}.tmp", encode(guid), std::process::id()));
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
        // Looked at again: keep it from being the next thing trimmed.
        crate::storage::touch(&path);
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

    /// A GIF whose canvas is `w` by `h` but whose frames are one pixel
    /// each, which is all it takes to ask for `w * h * 4` bytes a frame.
    fn sparse_gif(w: u16, h: u16, frames: usize) -> Vec<u8> {
        let mut bytes = b"GIF89a".to_vec();
        bytes.extend_from_slice(&w.to_le_bytes());
        bytes.extend_from_slice(&h.to_le_bytes());
        bytes.extend_from_slice(&[0x80, 0, 0, 0, 0, 0, 255, 255, 255]);
        for _ in 0..frames {
            bytes.extend_from_slice(&[0x21, 0xF9, 0x04, 0x00, 0x05, 0x00, 0x00, 0x00]);
            bytes.extend_from_slice(&[0x2C, 0, 0, 0, 0, 1, 0, 1, 0, 0x00]);
            bytes.extend_from_slice(&[0x02, 0x02, 0x44, 0x01, 0x00]);
        }
        bytes.push(0x3B);
        bytes
    }

    #[test]
    fn a_gif_cannot_ask_for_more_memory_than_allowed() {
        // 89 bytes that want 3.6 GB a frame: refused, not attempted.
        let huge = sparse_gif(30_000, 30_000, 3);
        assert!(huge.len() < 100);
        assert!(decode_gif(&huge, (100, 100)).is_err());
        // The largest canvas a GIF can state.
        assert!(decode_gif(&sparse_gif(u16::MAX, u16::MAX, 2), (100, 100)).is_err());
        // Nor may it be drawn as a still, which is what is tried next.
        assert!(decode(&huge).is_err());
        assert!(decode(&sparse_gif(u16::MAX, u16::MAX, 1)).is_err());
        assert!(decode(&sparse_gif(64, 64, 1)).is_ok());
        // An ordinary size with a great many frames plays, thinned, and
        // only so much of it is read.
        let long = sparse_gif(64, 64, MAX_GIF_FRAMES_READ + 500);
        let frames = decode_gif(&long, (100, 100)).unwrap().unwrap();
        assert!(frames.len() <= MAX_FRAMES && frames.len() > MAX_FRAMES / 2);
        let total: Duration = frames.iter().map(|(_, delay)| *delay).sum();
        assert_eq!(total, Duration::from_millis(50 * MAX_GIF_FRAMES_READ as u64));
    }

    #[test]
    fn pictures_cached_at_once_each_keep_their_own() {
        use image::RgbImage;
        let dir = std::env::temp_dir().join(format!("fizz-thumbs-test-{}", std::process::id()));
        let shade = |n: usize| (n * 9 % 250) as u8;
        let workers: Vec<_> = (0..4usize)
            .map(|worker| {
                let dir = dir.clone();
                std::thread::spawn(move || {
                    for n in (worker..200).step_by(4) {
                        let picture = RgbImage::from_pixel(16, 16, image::Rgb([shade(n); 3]));
                        write_cache_in(&dir, &format!("picture-{n}"), &DynamicImage::ImageRgb8(picture)).unwrap();
                    }
                })
            })
            .collect();
        for worker in workers {
            worker.join().unwrap();
        }
        for n in 0..200 {
            let bytes = std::fs::read(dir.join(format!("picture-{n}"))).unwrap();
            let pixel = decode(&bytes).unwrap().to_rgb8().get_pixel(8, 8).0[0];
            assert!(pixel.abs_diff(shade(n)) <= 3, "picture {n} holds another picture");
        }
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 200, "nothing left behind");
        std::fs::remove_dir_all(&dir).unwrap();
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
