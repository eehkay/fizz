//! Server address and password, read from the environment or
//! `~/.config/fizz/env`, with an interactive prompt on first run.

use anyhow::{Context, Result, bail};
use std::io::{self, BufRead, Write};
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

fn clean_url(url: &str) -> String {
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

fn prompt(label: &str) -> Result<String> {
    print!("{label}");
    io::stdout().flush()?;
    let mut line = String::new();
    io::stdin().lock().read_line(&mut line)?;
    Ok(line.trim().to_string())
}

/// Ask for the server details on the terminal and save them.
pub fn interactive_setup() -> Result<Config> {
    println!("Fizz needs to know where your BlueBubbles server is.\n");
    println!("  The address is whatever reaches the Mac running BlueBubbles Server,");
    println!("  for example http://my-mac:1234 or your proxy URL.");
    println!("  The password is the one set in BlueBubbles Server > Settings.\n");
    let url = prompt("Server address: ")?;
    let password = prompt("Server password: ")?;
    if url.is_empty() || password.is_empty() {
        bail!("both the address and the password are required");
    }
    let config = Config {
        url: clean_url(&url),
        password,
    };
    save(&config)?;
    println!("\nSaved to {}\n", config_path().display());
    Ok(config)
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
    if let Ok(mut file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(dir.join("fizz.log"))
    {
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
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Media {
    pub pictures: bool,
    pub picture_columns: u16,
    pub picture_rows: u16,
    pub video_stills: bool,
    pub video_still_limit_mb: u64,
    pub downloads: String,
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

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Settings {
    pub updates: Updates,
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
        }
    }
}

impl Default for Media {
    fn default() -> Self {
        Media {
            pictures: true,
            picture_columns: 38,
            picture_rows: 14,
            video_stills: true,
            video_still_limit_mb: 40,
            downloads: "~/Downloads".into(),
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

[media]
# Show pictures inline. When false they appear as a named row you can open.
pictures = true
# The largest a picture may be drawn, in characters.
picture_columns = 38
picture_rows = 14
# Show a still frame for videos (needs ffmpeg). The video is downloaded to
# make it, so there is a size limit, in megabytes.
video_stills = true
video_still_limit_mb = 40
# Where "s" saves pictures and files.
downloads = "~/Downloads"

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

fn loaded() -> &'static (Settings, Option<String>) {
    SETTINGS.get_or_init(|| {
        let path = settings_path();
        match std::fs::read_to_string(&path) {
            Ok(text) => match Settings::parse(&text) {
                Ok(settings) => (settings, None),
                Err(problem) => (Settings::default(), Some(problem)),
            },
            Err(_) => {
                // First run: leave the options where they can be found.
                if let Some(dir) = path.parent() {
                    let _ = std::fs::create_dir_all(dir);
                }
                let _ = std::fs::write(&path, SETTINGS_TEMPLATE);
                (Settings::default(), None)
            }
        }
    })
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
    fn rejects_typos_and_bad_values() {
        assert!(Settings::parse("[media]\npicturs = false\n").is_err());
        assert!(Settings::parse("[messages]\nsend_method = \"pigeon\"\n").is_err());
        assert!(Settings::parse("[appearance]\nclock = \"13h\"\n").is_err());
        let s = Settings::parse("[media]\npicture_rows = 9999\n").unwrap();
        assert_eq!(s.media.picture_rows, 80);
        assert!(s.notifications.enabled);
    }
}
