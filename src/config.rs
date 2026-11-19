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
