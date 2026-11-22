//! Server address and password, read from the environment or
//! `~/.config/fizz/env` (which `fizz setup` writes; see `setup.rs`).

use anyhow::{Context, Result, bail};
use std::io::Write;
use std::path::PathBuf;

#[derive(Debug, Clone)]
pub struct Config {
    pub url: String,
    pub password: String,
}

pub fn config_path() -> PathBuf {
    dirs::config_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("fizz")
        .join("env")
}

/// Where Fizz keeps what it has learned between runs, such as which
/// conversations you have already read.
pub fn state_dir() -> PathBuf {
    dirs::state_dir()
        .or_else(dirs::data_local_dir)
        .unwrap_or_else(std::env::temp_dir)
        .join("fizz")
}

pub fn cache_dir() -> PathBuf {
    dirs::cache_dir()
        .unwrap_or_else(std::env::temp_dir)
        .join("fizz")
}

fn parse(text: &str) -> (Option<String>, Option<String>) {
    let (mut url, mut password) = (None, None);
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let value = value.trim();
        let value = value
            .strip_prefix('"')
            .and_then(|v| v.strip_suffix('"'))
            .or_else(|| value.strip_prefix('\'').and_then(|v| v.strip_suffix('\'')))
            .unwrap_or(value)
            .to_string();
        match key.trim().trim_start_matches("export ").trim() {
            "BB_URL" => url = Some(value),
            "BB_PASSWORD" => password = Some(value),
            _ => {}
        }
    }
    (url, password)
}

pub fn clean_url(url: &str) -> String {
    let url = url.trim().trim_end_matches('/');
    if url.starts_with("http://") || url.starts_with("https://") {
        url.to_string()
    } else {
        format!("http://{url}")
    }
}

/// Load settings; environment variables win over the file.
pub fn load() -> Result<Option<Config>> {
    let (mut url, mut password) = match std::fs::read_to_string(config_path()) {
        Ok(text) => parse(&text),
        Err(_) => (None, None),
    };
    if let Ok(v) = std::env::var("BB_URL") {
        url = Some(v);
    }
    if let Ok(v) = std::env::var("BB_PASSWORD") {
        password = Some(v);
    }
    match (url, password) {
        (Some(url), Some(password)) if !url.is_empty() && !password.is_empty() => Ok(Some(Config {
            url: clean_url(&url),
            password,
        })),
        _ => Ok(None),
    }
}

pub fn save(config: &Config) -> Result<()> {
    let path = config_path();
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).context("creating the config directory")?;
    }
    let body = format!("BB_URL={}\nBB_PASSWORD={}\n", config.url, config.password);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&path)?;
        file.write_all(body.as_bytes())?;
    }
    #[cfg(not(unix))]
    std::fs::write(&path, body)?;
    Ok(())
}

/// Append a line to `~/.cache/fizz/fizz.log`. Used for failures that have
/// no room on screen; never for message content.
pub fn log(line: &str) {
    use std::io::Write as _;
    let dir = cache_dir();
    let _ = std::fs::create_dir_all(&dir);
    let mut options = std::fs::OpenOptions::new();
    options.create(true).append(true);
    // Errors are the server's and the system's own words, so the log is
    // kept to its owner like everything else Fizz writes about messages.
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    if let Ok(mut file) = options.open(dir.join("fizz.log")) {
        let _ = writeln!(file, "{} {line}", chrono::Local::now().format("%F %T"));
    }
}

// ----- preferences -------------------------------------------------------
//
// Everything besides the server's address and password lives in
// `~/.config/fizz/config.toml`, written out with its defaults on first run
// so the options can be found without reading documentation.

use serde::Deserialize;
use std::sync::OnceLock;

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Notifications {
    pub enabled: bool,
    /// Show the message text; when off, only who it is from.
    pub preview: bool,
    /// "off", "pop" (Fizz's own sound), or the path of a sound file.
    pub sound: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Appearance {
    /// "auto", "terminal", or the path of an Omarchy-style colors.toml.
    pub theme: String,
    /// "12h" or "24h".
    pub clock: String,
    pub list_width: u16,
    pub conversation_width: u16,
    pub mouse: bool,
    /// Colour your own bubbles by service: blue for iMessage, green for
    /// SMS and RCS.
    pub service_colors: bool,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Media {
    pub pictures: bool,
    pub picture_columns: u16,
    pub picture_rows: u16,
    /// Play GIFs in place, where the terminal makes that cheap (kitty,
    /// Ghostty). Elsewhere they show their first frame.
    pub animate_gifs: bool,
    pub video_stills: bool,
    pub video_still_limit_mb: u64,
    pub downloads: String,
    /// The most the picture cache (thumbnails and GIFs) may take, in
    /// megabytes. 0 is no limit.
    pub picture_cache_mb: u64,
    /// The same for opened files and videos.
    pub file_cache_mb: u64,
    /// Opened files not used for this many days are removed. 0 keeps
    /// them until the size limit pushes them out.
    pub file_cache_days: u64,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Messages {
    /// Added to phone numbers typed without a country code.
    pub default_country_code: String,
    /// "apple-script" or "private-api".
    pub send_method: String,
    pub emoji_popup: bool,
    pub poll_seconds: u64,
    /// Tell the server when a chat has been read (needs the Private API).
    pub mark_read: bool,
    /// Let the other person see "typing…" while you write (Private API).
    pub send_typing: bool,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Updates {
    /// Look for a newer version in the background.
    pub check: bool,
    pub every_hours: u64,
}

impl Default for Updates {
    fn default() -> Self {
        Updates {
            check: true,
            every_hours: 6,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Codes {
    /// Pick one-time codes out of incoming messages.
    pub detect: bool,
    /// Put a code on the clipboard the moment it arrives.
    pub auto_copy: bool,
}

impl Default for Codes {
    fn default() -> Self {
        Codes {
            detect: true,
            auto_copy: false,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Startup {
    /// Keep the chat list and the last few conversations between runs, so
    /// Fizz opens showing them at once instead of waiting for the server.
    pub remember: bool,
}

impl Default for Startup {
    fn default() -> Self {
        Startup { remember: true }
    }
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Search {
    /// How many years of message text to keep for searching by words.
    /// 0 keeps everything.
    pub years: u32,
    /// The most disk the search text may take, in megabytes. 0 is no
    /// limit.
    pub max_mb: u64,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Settings {
    pub updates: Updates,
    pub startup: Startup,
    pub search: Search,
    pub codes: Codes,
    pub notifications: Notifications,
    pub appearance: Appearance,
    pub media: Media,
    pub messages: Messages,
}

impl Default for Notifications {
    fn default() -> Self {
        Notifications {
            enabled: true,
            preview: true,
            sound: "off".into(),
        }
    }
}

impl Default for Appearance {
    fn default() -> Self {
        Appearance {
            theme: "auto".into(),
            clock: "12h".into(),
            list_width: 34,
            conversation_width: 110,
            mouse: true,
            service_colors: true,
        }
    }
}

impl Default for Media {
    fn default() -> Self {
        Media {
            pictures: true,
            picture_columns: 38,
            picture_rows: 14,
            animate_gifs: true,
            video_stills: true,
            video_still_limit_mb: 40,
            downloads: "~/Downloads".into(),
            picture_cache_mb: 150,
            file_cache_mb: 250,
            file_cache_days: 14,
        }
    }
}

impl Default for Messages {
    fn default() -> Self {
        Messages {
            default_country_code: "+1".into(),
            send_method: "apple-script".into(),
            emoji_popup: true,
            poll_seconds: 12,
            mark_read: true,
            send_typing: true,
        }
    }
}

/// What a fresh settings file looks like. Keep in step with the defaults
/// above; a test checks that it parses to exactly those.
pub const SETTINGS_TEMPLATE: &str = r#"# Fizz settings.
#
# Change a value and restart Fizz. Delete a line to go back to its default.
# The server address and password are kept separately, in the "env" file
# next to this one (change them with: fizz setup).

[updates]
# Look for a newer Fizz now and then, and show a banner when there is one.
# It asks the repository Fizz was installed from; nothing else is sent.
# Install an update with: fizz update
check = true
every_hours = 6

[startup]
# Open showing the chat list and conversations from last time straight
# away, then catch up with the server behind them. This keeps the last few
# conversations you opened (about 60 messages each) on this computer. When
# false, Fizz waits for the server before showing anything.
remember = true

[search]
# Searching messages by their words keeps a copy of message text on this
# computer (it is off until you turn it on in the search screen with
# Ctrl+B). It is small: roughly 14 MB for every 100,000 messages.
# See what it is using with: fizz storage    Remove it: fizz index delete
#
# Keep only this many years of it; older text is removed as it ages out.
# 0 keeps everything.
years = 0
# And never let it take more than this many megabytes; the oldest text
# goes first. 0 is no limit.
max_mb = 0

[codes]
# Spot one-time codes ("Your verification code is 482913") in messages that
# arrive. The code leads the notification, and Ctrl+Y copies the latest one.
detect = true
# Copy a code to the clipboard by itself the moment it arrives. Off by
# default because it replaces whatever you had copied, and clipboard
# managers keep a history.
auto_copy = false

[notifications]
# Desktop notifications for new messages in chats you are not looking at.
enabled = true
# Show the message text. Set to false to show only who it is from.
preview = true
# A sound for the same messages: "off", "pop" (a short bubble pop), or the
# path of a sound file of your own. Hear it with: fizz sound
sound = "off"

[appearance]
# "auto"     follow the current Omarchy theme, or the terminal if there is none
# "terminal" always use the terminal's own colours
# a path     use that Omarchy-style colors.toml, e.g. "~/themes/mine/colors.toml"
theme = "auto"
# "12h" (3:42 PM) or "24h" (15:42).
clock = "12h"
# Width of the chat list, in characters.
list_width = 34
# The conversation is kept to this width and centred on very wide windows.
conversation_width = 110
# Mouse clicks and wheel. Turn off to select text with the mouse as usual.
mouse = true
# Colour your own messages by how they were sent: blue for iMessage, green
# for SMS and RCS, using the theme's blue and green. When false they all
# use the theme's accent colour.
service_colors = true

[media]
# Show pictures inline. When false they appear as a named row you can open.
pictures = true
# The largest a picture may be drawn, in characters.
picture_columns = 38
picture_rows = 14
# Play GIFs in place. This works in kitty and Ghostty; other terminals show
# the first frame. They play only while the Fizz window is focused.
animate_gifs = true
# Show a still frame for videos (needs ffmpeg). The video is downloaded to
# make it, so there is a size limit, in megabytes.
video_stills = true
video_still_limit_mb = 40
# Where "s" saves pictures and files.
downloads = "~/Downloads"
# How much disk the caches may use. Each has its own limit, and when one is
# reached the least recently used things go first. 0 means no size limit.
# See what each is using with: fizz storage
# Pictures: a small copy of each picture you scroll past, and GIFs.
picture_cache_mb = 150
# Files: anything you open or save, and videos fetched for their still.
file_cache_mb = 250
# Files not used for this many days are removed whatever the size. 0 keeps
# them until the size limit pushes them out.
file_cache_days = 14

[messages]
# Added to phone numbers typed without a country code when starting a chat.
default_country_code = "+1"
# How the Mac sends: "apple-script" works everywhere; "private-api" needs
# the BlueBubbles Private API helper connected on the server.
send_method = "apple-script"
# The :name emoji popup while typing.
emoji_popup = true
# How often to check for anything the live connection missed, in seconds.
poll_seconds = 12
# When you open a chat, mark it read on the Mac too, so it clears on your
# other devices. Only possible with the Private API helper connected;
# without it Fizz just remembers what you have read on this computer.
mark_read = true
# Show the other person the "typing…" bubble while you write. Needs the
# Private API helper. Seeing theirs is not affected by this.
send_typing = true
"#;

pub fn settings_path() -> PathBuf {
    config_path().with_file_name("config.toml")
}

/// `~/x` as a real path.
pub fn expand_home(path: &str) -> PathBuf {
    match path.strip_prefix("~/") {
        Some(rest) => dirs::home_dir().unwrap_or_default().join(rest),
        None if path == "~" => dirs::home_dir().unwrap_or_default(),
        None => PathBuf::from(path),
    }
}

impl Settings {
    /// Parse, then pull anything silly back into a workable range.
    fn parse(text: &str) -> Result<Self, String> {
        let mut s: Settings = toml::from_str(text).map_err(|e| {
            // The first line of toml's message says what and where.
            e.to_string().lines().take(3).collect::<Vec<_>>().join(" ").trim().to_string()
        })?;
        s.appearance.list_width = s.appearance.list_width.clamp(20, 60);
        s.appearance.conversation_width = s.appearance.conversation_width.clamp(40, 400);
        s.media.picture_columns = s.media.picture_columns.clamp(8, 160);
        s.media.picture_rows = s.media.picture_rows.clamp(4, 80);
        s.messages.poll_seconds = s.messages.poll_seconds.clamp(3, 600);
        if !["apple-script", "private-api"].contains(&s.messages.send_method.as_str()) {
            return Err(format!(
                "send_method must be \"apple-script\" or \"private-api\", not \"{}\"",
                s.messages.send_method
            ));
        }
        if !["12h", "24h"].contains(&s.appearance.clock.as_str()) {
            return Err(format!(
                "clock must be \"12h\" or \"24h\", not \"{}\"",
                s.appearance.clock
            ));
        }
        Ok(s)
    }
}

static SETTINGS: OnceLock<(Settings, Option<String>)> = OnceLock::new();

/// Read the settings file at `path`: the settings to use, and what is
/// wrong with the file if it could not be used. Only a file that is not
/// there is written out afresh. One that is there but cannot be read (a
/// stray character that is not text, say) is somebody's settings, and is
/// left exactly as it is.
fn read_settings(path: &std::path::Path) -> (Settings, Option<String>) {
    match std::fs::read_to_string(path) {
        Ok(text) => match Settings::parse(&text) {
            Ok(settings) => (settings, None),
            Err(problem) => (Settings::default(), Some(problem)),
        },
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            // First run: leave the options where they can be found.
            if let Some(dir) = path.parent() {
                let _ = std::fs::create_dir_all(dir);
            }
            let _ = std::fs::write(path, SETTINGS_TEMPLATE);
            (Settings::default(), None)
        }
        Err(e) => (Settings::default(), Some(format!("it could not be read ({e})"))),
    }
}

fn loaded() -> &'static (Settings, Option<String>) {
    SETTINGS.get_or_init(|| read_settings(&settings_path()))
}

/// The preferences in force. Read once, at first use.
pub fn settings() -> &'static Settings {
    &loaded().0
}

/// What is wrong with the settings file, if it could not be used.
pub fn settings_problem() -> Option<&'static str> {
    loaded().1.as_deref()
}

/// Open the settings file in the user's editor.
pub fn edit_settings() -> Result<()> {
    let path = settings_path();
    loaded();
    if !path.exists() {
        std::fs::write(&path, SETTINGS_TEMPLATE)?;
    }
    let editor = std::env::var("VISUAL")
        .or_else(|_| std::env::var("EDITOR"))
        .ok()
        .filter(|e| !e.trim().is_empty());
    match editor {
        Some(editor) => {
            // Editors are often given with arguments, e.g. "code --wait".
            let mut parts = editor.split_whitespace();
            let program = parts.next().unwrap_or("vi");
            let status = std::process::Command::new(program)
                .args(parts)
                .arg(&path)
                .status()
                .with_context(|| format!("couldn't start {program}"))?;
            if !status.success() {
                bail!("{program} exited with an error");
            }
            let text = std::fs::read_to_string(&path)?;
            match Settings::parse(&text) {
                Ok(_) => println!("Saved. Restart Fizz to apply."),
                Err(problem) => println!("There is a problem in the file, so defaults will be used:\n  {problem}"),
            }
        }
        None => println!(
            "Settings are in {}\n(set EDITOR to have this command open it for you)",
            path.display()
        ),
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn template_matches_defaults() {
        let parsed = Settings::parse(SETTINGS_TEMPLATE).unwrap();
        assert_eq!(format!("{parsed:?}"), format!("{:?}", Settings::default()));
    }

    #[test]
    fn a_settings_file_that_cannot_be_read_is_left_alone() {
        let dir = std::env::temp_dir().join(format!("fizz-settings-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        // Missing: written out with the defaults, and nothing to report.
        let fresh = dir.join("fresh.toml");
        let (settings, problem) = read_settings(&fresh);
        assert!(problem.is_none() && settings.media.pictures);
        assert_eq!(std::fs::read_to_string(&fresh).unwrap(), SETTINGS_TEMPLATE);
        // There, but not text: reported, defaults used, file untouched.
        let odd = dir.join("odd.toml");
        let mine = b"# caf\xe9\n[media]\npictures = false\n".to_vec();
        std::fs::write(&odd, &mine).unwrap();
        let (settings, problem) = read_settings(&odd);
        assert!(settings.media.pictures);
        assert!(problem.is_some_and(|p| p.contains("could not be read")));
        assert_eq!(std::fs::read(&odd).unwrap(), mine);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn rejects_typos_and_bad_values() {
        assert!(Settings::parse("[media]\npicturs = false\n").is_err());
        assert!(Settings::parse("[messages]\nsend_method = \"pigeon\"\n").is_err());
        assert!(Settings::parse("[appearance]\nclock = \"13h\"\n").is_err());
        let s = Settings::parse("[media]\npicture_rows = 9999\n").unwrap();
        assert_eq!(s.media.picture_rows, 80);
        assert!(s.notifications.enabled);
    }
}
