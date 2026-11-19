//! Opening and saving attachments: pictures at full size, videos, audio and
//! any other file, handed to whatever app the desktop uses for that type.

use anyhow::{Context, Result, anyhow};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, SystemTime};
use tokio::sync::mpsc::UnboundedSender;

use crate::api::{Api, encode};
use crate::app::AppEvent;
use crate::config::cache_dir;
use crate::model::Attachment;

/// Downloaded files are kept this long so reopening one is instant.
const KEEP_FOR: Duration = Duration::from_secs(14 * 24 * 3600);

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Then {
    Open,
    Save,
}

fn files_dir() -> PathBuf {
    cache_dir().join("files")
}

/// A file name that is safe to create, whatever the sender called it.
fn safe_name(name: &str) -> String {
    let cleaned: String = name
        .chars()
        .map(|c| if c == '/' || c == '\\' || c.is_control() { '_' } else { c })
        .collect();
    let cleaned = cleaned.trim().trim_start_matches('.').to_string();
    if cleaned.is_empty() {
        "attachment".into()
    } else {
        cleaned
    }
}

/// The server converts some Apple formats on the way out (HEIC to JPEG, CAF
/// to MP3) but the name still carries the old extension. Go by the contents.
fn fix_extension(name: &str, head: &[u8]) -> String {
    let actual = if head.starts_with(&[0xFF, 0xD8, 0xFF]) {
        "jpg"
    } else if head.starts_with(b"\x89PNG") {
        "png"
    } else if head.starts_with(b"ID3") || head.starts_with(&[0xFF, 0xFB]) {
        "mp3"
    } else {
        return name.to_string();
    };
    let path = Path::new(name);
    let current = path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_lowercase();
    let same = current == actual || (actual == "jpg" && current == "jpeg");
    if same {
        return name.to_string();
    }
    let stem = path.file_stem().and_then(|s| s.to_str()).unwrap_or(name);
    format!("{stem}.{actual}")
}

fn existing(dir: &Path) -> Option<PathBuf> {
    std::fs::read_dir(dir)
        .ok()?
        .flatten()
        .map(|entry| entry.path())
        .find(|p| p.is_file() && p.extension().is_none_or(|e| e != "part"))
}

/// The attachment on local disk, downloading it first if needed. `tx`
/// receives progress lines for the status bar; pass `None` to stay quiet.
pub async fn local_copy(
    api: &Api,
    attachment: &Attachment,
    tx: Option<&UnboundedSender<AppEvent>>,
) -> Result<PathBuf> {
    let dir = files_dir().join(encode(&attachment.guid));
    if let Some(path) = existing(&dir) {
        return Ok(path);
    }
    tokio::fs::create_dir_all(&dir).await?;
    let part = dir.join("download.part");
    let name = safe_name(attachment.name());

    let expected = attachment.total_bytes;
    let mut last_step = 0;
    let label = name.clone();
    let progress_tx = tx.cloned();
    api.download_to(&attachment.guid, &part, move |done, total| {
        let Some(progress_tx) = &progress_tx else {
            return;
        };
        // Report in tenths, and only for files big enough to make you wait.
        let Some(total) = total.or(expected).filter(|t| *t > 3_000_000) else {
            return;
        };
        let step = (done * 10 / total.max(1)).min(10);
        if step != last_step {
            last_step = step;
            let _ = progress_tx.send(AppEvent::Status(format!(
                "Downloading {label}… {}%",
                step * 10
            )));
        }
    })
    .await?;

    let mut head = [0u8; 8];
    let read = {
        use tokio::io::AsyncReadExt;
        let mut file = tokio::fs::File::open(&part).await?;
        file.read(&mut head).await?
    };
    let path = dir.join(fix_extension(&name, &head[..read]));
    tokio::fs::rename(&part, &path).await?;
    Ok(path)
}

/// Hand a file or web address to the desktop's default app for it.
pub fn open(path: impl AsRef<std::ffi::OsStr>) -> Result<()> {
    let mut command = if cfg!(target_os = "macos") {
        Command::new("open")
    } else if cfg!(windows) {
        let mut c = Command::new("cmd");
        c.args(["/C", "start", ""]);
        c
    } else {
        Command::new("xdg-open")
    };
    let mut child = command
        .arg(path.as_ref())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .context("couldn't start the desktop's file opener")?;
    // Collect the exit status in the background so no zombie is left.
    std::thread::spawn(move || {
        let _ = child.wait();
    });
    Ok(())
}

fn downloads_dir() -> PathBuf {
    let chosen = crate::config::settings().media.downloads.trim();
    if chosen.is_empty() || chosen == "~/Downloads" {
        // The default follows the desktop's own Downloads folder.
        return dirs::download_dir()
            .or_else(|| dirs::home_dir().map(|h| h.join("Downloads")))
            .unwrap_or_else(|| PathBuf::from("."));
    }
    crate::config::expand_home(chosen)
}

/// `name.ext`, or `name (2).ext` and so on if that is taken.
fn unused_path(dir: &Path, name: &str) -> PathBuf {
    let first = dir.join(name);
    if !first.exists() {
        return first;
    }
    let path = Path::new(name);
    let stem = path.file_stem().and_then(|s| s.to_str()).unwrap_or(name);
    let ext = path.extension().and_then(|e| e.to_str());
    (2..)
        .map(|n| match ext {
            Some(ext) => dir.join(format!("{stem} ({n}).{ext}")),
            None => dir.join(format!("{stem} ({n})")),
        })
        .find(|p| !p.exists())
        .expect("an unused name exists")
}

fn display(path: &Path) -> String {
    match dirs::home_dir().and_then(|home| path.strip_prefix(home).ok().map(Path::to_path_buf)) {
        Some(rest) => format!("~/{}", rest.display()),
        None => path.display().to_string(),
    }
}

async fn run(
    api: &Api,
    attachment: &Attachment,
    then: Then,
    tx: &UnboundedSender<AppEvent>,
) -> Result<String> {
    let path = local_copy(api, attachment, Some(tx)).await?;
    let name = path
        .file_name()
        .and_then(|n| n.to_str())
        .ok_or_else(|| anyhow!("bad file name"))?
        .to_string();
    match then {
        Then::Open => {
            open(&path)?;
            Ok(format!("Opened {name}"))
        }
        Then::Save => {
            let dir = downloads_dir();
            tokio::fs::create_dir_all(&dir).await?;
            let destination = unused_path(&dir, &name);
            tokio::fs::copy(&path, &destination).await?;
            Ok(format!("Saved to {}", display(&destination)))
        }
    }
}

/// Download (if needed) then open or save, reporting back to the app.
pub fn fetch(api: Api, attachment: Attachment, then: Then, tx: UnboundedSender<AppEvent>) {
    tokio::spawn(async move {
        let result = run(&api, &attachment, then, &tx)
            .await
            .map_err(|e| format!("{e:#}"));
        let _ = tx.send(AppEvent::FileDone {
            guid: attachment.guid,
            result,
        });
    });
}

fn percent_decode(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        let hex = (bytes[i] == b'%')
            .then(|| text.get(i + 1..i + 3))
            .flatten()
            .and_then(|h| u8::from_str_radix(h, 16).ok());
        match hex {
            Some(byte) => {
                out.push(byte);
                i += 3;
            }
            None => {
                out.push(bytes[i]);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Split like a shell would: quotes group, backslash escapes.
fn shell_split(line: &str) -> Option<Vec<String>> {
    let mut words = Vec::new();
    let mut word = String::new();
    let mut in_word = false;
    let mut chars = line.chars();
    while let Some(c) = chars.next() {
        match c {
            '\'' => {
                in_word = true;
                loop {
                    match chars.next()? {
                        '\'' => break,
                        other => word.push(other),
                    }
                }
            }
            '"' => {
                in_word = true;
                loop {
                    match chars.next()? {
                        '"' => break,
                        '\\' => word.push(chars.next()?),
                        other => word.push(other),
                    }
                }
            }
            '\\' => {
                in_word = true;
                word.push(chars.next()?);
            }
            c if c.is_whitespace() => {
                if in_word {
                    words.push(std::mem::take(&mut word));
                    in_word = false;
                }
            }
            other => {
                in_word = true;
                word.push(other);
            }
        }
    }
    if in_word {
        words.push(word);
    }
    Some(words)
}

fn existing_file(text: &str) -> Option<PathBuf> {
    let text = match text.strip_prefix("file://") {
        Some(rest) => percent_decode(rest.strip_prefix("localhost").unwrap_or(rest)),
        None => text.to_string(),
    };
    let path = match text.strip_prefix("~/") {
        Some(rest) => dirs::home_dir()?.join(rest),
        None => PathBuf::from(text),
    };
    (path.is_absolute() && path.is_file()).then_some(path)
}

/// When pasted text is nothing but paths to files that exist, return them.
/// That is what a terminal sends when files are dragged onto it, whether as
/// plain paths, shell-quoted paths or `file://` addresses.
pub fn dropped_paths(text: &str) -> Option<Vec<PathBuf>> {
    // Terminals usually paste line breaks as carriage returns.
    let text = text.replace("\r\n", "\n").replace('\r', "\n");
    let text = text.trim();
    if text.is_empty() || text.len() > 32_768 {
        return None;
    }
    if let Some(path) = existing_file(text) {
        return Some(vec![path]);
    }
    let mut found = Vec::new();
    for line in text.lines().map(str::trim).filter(|l| !l.is_empty()) {
        if let Some(path) = existing_file(line) {
            found.push(path);
            continue;
        }
        let words = shell_split(line)?;
        if words.is_empty() {
            return None;
        }
        for word in words {
            found.push(existing_file(&word)?);
        }
    }
    (!found.is_empty()).then_some(found)
}

/// Show a desktop notification. Best effort: silently does nothing when
/// the desktop has no notifier.
pub fn notify(title: &str, body: &str) {
    let mut command = if cfg!(target_os = "macos") {
        let script = format!(
            "display notification {body:?} with title {title:?}",
            body = body,
            title = title
        );
        let mut c = Command::new("osascript");
        c.args(["-e", &script]);
        c
    } else {
        let mut c = Command::new("notify-send");
        c.args(["--app-name", "Fizz", "--icon", "fizz", "--", title, body]);
        c
    };
    if let Ok(mut child) = command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
    {
        std::thread::spawn(move || {
            let _ = child.wait();
        });
    }
}

/// Fizz's own notification sound: a bubble pop, which is a quick upward
/// sweep in pitch that dies away. Built here so nothing has to be shipped
/// or found on the system. Returns a 16-bit mono WAV.
fn pop_wav() -> Vec<u8> {
    const RATE: u32 = 44_100;
    let samples = (RATE as f32 * 0.17) as usize;
    let mut pcm = Vec::with_capacity(samples * 2);
    let mut phase = 0.0f32;
    for i in 0..samples {
        let t = i as f32 / RATE as f32;
        let progress = i as f32 / samples as f32;
        // 480 Hz rising to about 1250 Hz.
        let pitch = 480.0 * (2.6f32).powf(progress);
        phase += std::f32::consts::TAU * pitch / RATE as f32;
        // Fast attack so it does not click, then a quick fade.
        let envelope = (t / 0.006).min(1.0) * (-progress * 5.5).exp();
        let value = phase.sin() * envelope * 0.42;
        pcm.extend_from_slice(&((value * i16::MAX as f32) as i16).to_le_bytes());
    }
    let mut wav = Vec::with_capacity(44 + pcm.len());
    wav.extend_from_slice(b"RIFF");
    wav.extend_from_slice(&(36 + pcm.len() as u32).to_le_bytes());
    wav.extend_from_slice(b"WAVEfmt ");
    wav.extend_from_slice(&16u32.to_le_bytes());
    wav.extend_from_slice(&1u16.to_le_bytes()); // PCM
    wav.extend_from_slice(&1u16.to_le_bytes()); // mono
    wav.extend_from_slice(&RATE.to_le_bytes());
    wav.extend_from_slice(&(RATE * 2).to_le_bytes());
    wav.extend_from_slice(&2u16.to_le_bytes());
    wav.extend_from_slice(&16u16.to_le_bytes());
    wav.extend_from_slice(b"data");
    wav.extend_from_slice(&(pcm.len() as u32).to_le_bytes());
    wav.extend_from_slice(&pcm);
    wav
}

/// Play the notification sound chosen in the settings ("pop" or a file).
/// Best effort: returns whether a player could be started.
pub fn play_sound(choice: &str) -> bool {
    let path = match choice.trim() {
        "" | "off" => return false,
        "pop" => {
            let path = cache_dir().join("pop.wav");
            if !path.exists() {
                let _ = std::fs::create_dir_all(cache_dir());
                if std::fs::write(&path, pop_wav()).is_err() {
                    return false;
                }
            }
            path
        }
        file => crate::config::expand_home(file),
    };
    if !path.is_file() {
        return false;
    }
    // Whichever player this system has; the first that starts wins.
    let players: [(&str, &[&str]); 5] = [
        ("pw-play", &[]),
        ("paplay", &[]),
        ("afplay", &[]),
        ("ffplay", &["-nodisp", "-autoexit", "-loglevel", "quiet"]),
        ("aplay", &["-q"]),
    ];
    for (player, args) in players {
        if let Ok(mut child) = Command::new(player)
            .args(args)
            .arg(&path)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
        {
            std::thread::spawn(move || {
                let _ = child.wait();
            });
            return true;
        }
    }
    false
}

/// Put text on the clipboard: through the desktop's own tool when there is
/// one, otherwise by asking the terminal to do it.
pub fn copy_text(text: &str) -> Result<()> {
    use std::io::Write;
    let tools: [(&str, &[&str]); 3] = [
        ("wl-copy", &[]),
        ("pbcopy", &[]),
        ("xclip", &["-selection", "clipboard"]),
    ];
    for (tool, args) in tools {
        let Ok(mut child) = Command::new(tool)
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
        else {
            continue;
        };
        if let Some(mut stdin) = child.stdin.take() {
            let _ = stdin.write_all(text.as_bytes());
        }
        if child.wait().is_ok_and(|status| status.success()) {
            return Ok(());
        }
    }
    // OSC 52: understood by kitty, Ghostty, foot, WezTerm and others, and
    // works over SSH too.
    use base64::Engine;
    let encoded = base64::engine::general_purpose::STANDARD.encode(text);
    let mut out = std::io::stdout();
    write!(out, "\x1b]52;c;{encoded}\x07")?;
    out.flush()?;
    Ok(())
}

pub enum Clipboard {
    Picture(PathBuf),
    Text(String),
    Empty,
}

/// Read the Wayland clipboard: a picture is written to a file ready to
/// attach, text is returned as is. Blocking; call off the UI thread.
pub fn read_clipboard() -> Result<Clipboard> {
    let run = |args: &[&str]| -> Result<Vec<u8>> {
        let output = Command::new("wl-paste")
            .args(args)
            .stdin(Stdio::null())
            .stderr(Stdio::null())
            .output()
            .context("pasting needs wl-paste (the wl-clipboard package)")?;
        Ok(if output.status.success() {
            output.stdout
        } else {
            Vec::new()
        })
    };
    let types = String::from_utf8_lossy(&run(&["--list-types"])?).into_owned();
    let picture = ["image/png", "image/jpeg", "image/gif", "image/webp"]
        .into_iter()
        .find(|kind| types.lines().any(|line| line.trim() == *kind));
    if let Some(kind) = picture {
        let bytes = run(&["--type", kind])?;
        if !bytes.is_empty() {
            let dir = cache_dir().join("outbox");
            std::fs::create_dir_all(&dir)?;
            let ext = kind.rsplit('/').next().unwrap_or("png").replace("jpeg", "jpg");
            let stamp = chrono::Local::now().format("%Y-%m-%d %H.%M.%S");
            let path = unused_path(&dir, &format!("Pasted {stamp}.{ext}"));
            std::fs::write(&path, bytes)?;
            return Ok(Clipboard::Picture(path));
        }
    }
    if types.lines().any(|line| line.starts_with("text/")) {
        let text = String::from_utf8_lossy(&run(&["--no-newline"])?).into_owned();
        if !text.is_empty() {
            return Ok(Clipboard::Text(text));
        }
    }
    Ok(Clipboard::Empty)
}

/// Picture thumbnails are tiny, but there is one for every picture ever
/// scrolled past. Drop those not needed for three months; they are fetched
/// again if that conversation is opened.
fn prune_thumbnails() {
    const KEEP: Duration = Duration::from_secs(90 * 24 * 3600);
    let Ok(entries) = std::fs::read_dir(cache_dir().join("thumbs")) else {
        return;
    };
    let now = SystemTime::now();
    for entry in entries.flatten() {
        let stale = entry
            .metadata()
            .and_then(|m| m.modified())
            .ok()
            .and_then(|modified| now.duration_since(modified).ok())
            .is_some_and(|age| age > KEEP);
        if stale {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}

/// Drop downloads that have not been touched for a while.
pub fn prune() {
    prune_thumbnails();
    let now = SystemTime::now();
    let entries = [files_dir(), cache_dir().join("outbox")]
        .into_iter()
        .filter_map(|dir| std::fs::read_dir(dir).ok())
        .flatten();
    for entry in entries.flatten() {
        let old = entry
            .metadata()
            .and_then(|m| m.modified())
            .ok()
            .and_then(|modified| now.duration_since(modified).ok())
            .is_some_and(|age| age > KEEP_FOR);
        if old {
            let path = entry.path();
            let _ = if path.is_dir() {
                std::fs::remove_dir_all(path)
            } else {
                std::fs::remove_file(path)
            };
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_are_made_safe() {
        assert_eq!(safe_name("../../etc/passwd"), "_.._etc_passwd");
        assert_eq!(safe_name("  "), "attachment");
        assert_eq!(safe_name("IMG_1.heic"), "IMG_1.heic");
    }

    #[test]
    fn recognises_dropped_files() {
        let dir = std::env::temp_dir().join(format!("fizz-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let plain = dir.join("a.txt");
        let spaced = dir.join("b c's.png");
        std::fs::write(&plain, "x").unwrap();
        std::fs::write(&spaced, "x").unwrap();
        let (p, s) = (plain.to_str().unwrap(), spaced.to_str().unwrap());

        // One raw path, even with spaces in it.
        assert_eq!(dropped_paths(s), Some(vec![spaced.clone()]));
        // One per line, as kitty sends a multi-file drop.
        assert_eq!(dropped_paths(&format!("{p}\n{s}\n")), Some(vec![plain.clone(), spaced.clone()]));
        // The same with the carriage returns a terminal paste really uses.
        assert_eq!(dropped_paths(&format!("{p}\r{s}\r")), Some(vec![plain.clone(), spaced.clone()]));
        // Shell-quoted on one line.
        let quoted = format!("{p} \"{}\"", s);
        assert_eq!(dropped_paths(&quoted), Some(vec![plain.clone(), spaced.clone()]));
        let escaped = s.replace(' ', "\\ ").replace('\'', "\\'");
        assert_eq!(dropped_paths(&escaped), Some(vec![spaced.clone()]));
        // file:// addresses.
        let uri = format!("file://{}", s.replace(' ', "%20"));
        assert_eq!(dropped_paths(&uri), Some(vec![spaced.clone()]));
        // Ordinary text, or a path that is not there, stays text.
        assert_eq!(dropped_paths("see you at /tmp tomorrow"), None);
        assert_eq!(dropped_paths(&format!("{p} and more")), None);
        assert_eq!(dropped_paths("/no/such/file.png"), None);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn pop_is_a_valid_short_wav() {
        let wav = pop_wav();
        assert_eq!(&wav[..4], b"RIFF");
        assert_eq!(&wav[8..12], b"WAVE");
        let declared = u32::from_le_bytes(wav[40..44].try_into().unwrap()) as usize;
        assert_eq!(declared, wav.len() - 44);
        // Under a quarter of a second, and it ends quietly (no click).
        assert!(declared < 44_100 * 2 / 4);
        let last = i16::from_le_bytes(wav[wav.len() - 2..].try_into().unwrap());
        assert!(last.abs() < 400);
    }

    #[test]
    fn extension_follows_contents() {
        assert_eq!(fix_extension("IMG_1.heic", &[0xFF, 0xD8, 0xFF, 0xE0]), "IMG_1.jpg");
        assert_eq!(fix_extension("IMG_1.jpeg", &[0xFF, 0xD8, 0xFF, 0xE0]), "IMG_1.jpeg");
        assert_eq!(fix_extension("clip.mov", b"\0\0\0\x14ftyp"), "clip.mov");
        assert_eq!(fix_extension("Audio Message.caf", b"ID3\x04"), "Audio Message.mp3");
    }
}
