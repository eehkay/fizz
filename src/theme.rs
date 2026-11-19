//! Colours. On Omarchy the current theme's `colors.toml` is used and followed
//! live; anywhere else the terminal's own 16 colours are used.

use ratatui::style::Color;
use std::path::PathBuf;
use std::time::Duration;
use tokio::sync::mpsc::UnboundedSender;

use crate::app::AppEvent;

#[derive(Debug, Clone, PartialEq)]
pub struct Theme {
    pub name: String,
    pub accent: Color,
    /// Text drawn on top of `accent`.
    pub on_accent: Color,
    /// Raised surface: incoming bubbles and the selected row.
    pub surface: Color,
    pub text: Color,
    pub bright: Color,
    pub muted: Color,
    pub red: Color,
    pub green: Color,
    pub yellow: Color,
}

type Rgb = (u8, u8, u8);

fn parse_hex(s: &str) -> Option<Rgb> {
    let s = s.trim().trim_start_matches('#');
    if s.len() != 6 {
        return None;
    }
    let n = u32::from_str_radix(s, 16).ok()?;
    Some(((n >> 16) as u8, (n >> 8) as u8, n as u8))
}

fn mix(a: Rgb, b: Rgb, t: f32) -> Rgb {
    let f = |x: u8, y: u8| (x as f32 + (y as f32 - x as f32) * t).round() as u8;
    (f(a.0, b.0), f(a.1, b.1), f(a.2, b.2))
}

fn luminance(c: Rgb) -> f32 {
    let f = |v: u8| {
        let v = v as f32 / 255.0;
        if v <= 0.03928 {
            v / 12.92
        } else {
            ((v + 0.055) / 1.055).powf(2.4)
        }
    };
    0.2126 * f(c.0) + 0.7152 * f(c.1) + 0.0722 * f(c.2)
}

fn contrast(a: Rgb, b: Rgb) -> f32 {
    let (la, lb) = (luminance(a), luminance(b));
    (la.max(lb) + 0.05) / (la.min(lb) + 0.05)
}

fn rgb(c: Rgb) -> Color {
    Color::Rgb(c.0, c.1, c.2)
}

impl Theme {
    /// Works in any terminal: defers to whatever palette the terminal has.
    pub fn ansi() -> Self {
        Theme {
            name: "terminal".into(),
            accent: Color::Blue,
            on_accent: Color::Black,
            surface: Color::Indexed(237),
            text: Color::Reset,
            bright: Color::White,
            muted: Color::DarkGray,
            red: Color::Red,
            green: Color::Green,
            yellow: Color::Yellow,
        }
    }

    /// Build from the text of an Omarchy `colors.toml`. Only `background`,
    /// `foreground` and `accent` are required; the rest is derived if absent.
    pub fn from_colors_toml(text: &str, name: &str) -> Option<Self> {
        let table: toml::Table = text.parse().ok()?;
        let get = |key: &str| table.get(key).and_then(|v| v.as_str()).and_then(parse_hex);

        let bg = get("background")?;
        let fg = get("foreground")?;
        let accent = get("accent").or_else(|| get("blue"))?;

        // Themes disagree on how visible "muted" and "lighter_background" are,
        // so nudge them until they are usable for text and for a surface.
        let mut muted = get("muted").unwrap_or_else(|| mix(bg, fg, 0.5));
        if contrast(muted, bg) < 3.0 {
            muted = mix(bg, fg, 0.55);
        }
        let mut surface = get("lighter_background").unwrap_or_else(|| mix(bg, fg, 0.12));
        if contrast(surface, bg) < 1.12 {
            surface = mix(bg, fg, 0.12);
        }
        let bright = get("bright_foreground").unwrap_or(fg);
        let on_accent = if contrast(bg, accent) >= contrast(bright, accent) {
            bg
        } else {
            bright
        };

        Some(Theme {
            name: name.to_string(),
            accent: rgb(accent),
            on_accent: rgb(on_accent),
            surface: rgb(surface),
            text: rgb(fg),
            bright: rgb(bright),
            muted: rgb(muted),
            red: get("red").map_or(Color::Red, rgb),
            green: get("green").map_or(Color::Green, rgb),
            yellow: get("yellow").map_or(Color::Yellow, rgb),
        })
    }

    /// The theme to use right now.
    pub fn load() -> Self {
        match choice().as_str() {
            "auto" => {}
            "terminal" | "ansi" => return Theme::ansi(),
            path => {
                if let Some(t) = std::fs::read_to_string(crate::config::expand_home(path))
                    .ok()
                    .and_then(|text| Theme::from_colors_toml(&text, "custom"))
                {
                    return t;
                }
            }
        }
        let Some(dir) = omarchy_current_dir() else {
            return Theme::ansi();
        };
        let name = std::fs::read_to_string(dir.join("theme.name"))
            .map(|s| s.trim().to_string())
            .unwrap_or_else(|_| "omarchy".into());
        std::fs::read_to_string(dir.join("theme").join("colors.toml"))
            .ok()
            .and_then(|text| Theme::from_colors_toml(&text, &name))
            .unwrap_or_else(Theme::ansi)
    }
}

/// Which theme was asked for: the FIZZ_THEME variable if set, otherwise the
/// settings file. "auto" means follow Omarchy.
fn choice() -> String {
    std::env::var("FIZZ_THEME")
        .ok()
        .filter(|v| !v.trim().is_empty())
        .unwrap_or_else(|| crate::config::settings().appearance.theme.clone())
        .trim()
        .to_string()
}

fn omarchy_current_dir() -> Option<PathBuf> {
    let state = dirs::state_dir().or_else(|| dirs::home_dir().map(|h| h.join(".local/state")))?;
    let dir = state.join("omarchy").join("current");
    dir.is_dir().then_some(dir)
}

/// A cheap fingerprint that changes whenever Omarchy switches theme.
fn fingerprint() -> Option<String> {
    let dir = omarchy_current_dir()?;
    let generation = std::fs::read_to_string(dir.join("theme.generation")).unwrap_or_default();
    let modified = std::fs::metadata(dir.join("theme").join("colors.toml"))
        .and_then(|m| m.modified())
        .ok();
    Some(format!("{generation}|{modified:?}"))
}

/// Tell the app when the Omarchy theme changes. Does nothing elsewhere.
pub fn watch(tx: UnboundedSender<AppEvent>) {
    if choice() != "auto" {
        return;
    }
    tokio::spawn(async move {
        let mut last = fingerprint();
        let mut tick = tokio::time::interval(Duration::from_millis(750));
        loop {
            tick.tick().await;
            let now = fingerprint();
            if now != last {
                // Give Omarchy a moment to finish writing the new theme.
                tokio::time::sleep(Duration::from_millis(250)).await;
                last = fingerprint();
                if tx.send(AppEvent::Theme(Theme::load())).is_err() {
                    break;
                }
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_minimal_theme() {
        let t = Theme::from_colors_toml(
            "background = \"#000000\"\nforeground = \"#ffffff\"\naccent = \"#3080d0\"\n",
            "x",
        )
        .unwrap();
        assert_eq!(t.accent, Color::Rgb(0x30, 0x80, 0xd0));
        assert_ne!(t.surface, Color::Rgb(0, 0, 0));
    }

    #[test]
    fn rejects_garbage() {
        assert!(Theme::from_colors_toml("nope", "x").is_none());
        assert!(Theme::from_colors_toml("background = 3", "x").is_none());
    }
}
