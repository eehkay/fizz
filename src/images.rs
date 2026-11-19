//! Downloading, caching and shrinking picture attachments off the UI thread.

use anyhow::{Context, Result};
use image::{DynamicImage, ImageDecoder, ImageFormat, ImageReader};
use ratatui_image::sliced::SlicedProtocol;
use std::io::Cursor;
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
    Failed,
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
    limit: Arc<Semaphore>,
    tx: UnboundedSender<AppEvent>,
) {
    tokio::spawn(async move {
        let _permit = limit.acquire().await;
        let guid = attachment.guid.clone();
        let result = load(&api, &attachment, box_px).await.map_err(|e| {
            crate::config::log(&format!("picture {guid} failed: {e:#}"));
            e.to_string()
        });
        let _ = tx.send(AppEvent::Image { guid, result });
    });
}
