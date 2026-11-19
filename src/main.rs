//! Fizz: a terminal client for iMessage through a BlueBubbles server.

mod api;
mod app;
mod config;
mod demo;
mod emoji;
mod files;
mod images;
mod input;
mod live;
mod model;
mod theme;
mod ui;

use anyhow::{Context, Result};
use futures_util::StreamExt;
use ratatui::backend::TestBackend;
use ratatui::crossterm::event::{
    DisableBracketedPaste, DisableFocusChange, DisableMouseCapture, EnableBracketedPaste,
    EnableFocusChange, EnableMouseCapture,
    EventStream, KeyboardEnhancementFlags, PopKeyboardEnhancementFlags,
    PushKeyboardEnhancementFlags,
};
use ratatui::crossterm::execute;
use ratatui::{DefaultTerminal, Terminal};
use ratatui_image::picker::Picker;
use std::io::stdout;
use std::time::Duration;
use tokio::sync::mpsc::unbounded_channel;

use crate::api::Api;
use crate::app::App;
use crate::config::Config;

const HELP: &str = "\
Fizz — iMessage in your terminal, through a BlueBubbles server

Usage:
  fizz            Open the app (asks for server details the first time)
  fizz setup      Enter or change the server address and password
  fizz check      Test the connection and print what was found
  fizz config     Open the settings file in your editor
  fizz --demo     Try the interface with made-up conversations

The server address and password live in ~/.config/fizz/env (BB_URL and
BB_PASSWORD); environment variables of the same names override the file.
Everything else (notifications, colours, picture sizes, clock, and so on)
is in ~/.config/fizz/config.toml, which lists every option with its default.

Colours follow the current Omarchy theme when there is one, and your
terminal's own colours otherwise. FIZZ_THEME=ansi forces the latter, and
FIZZ_THEME=/path/to/colors.toml uses a specific palette.

Pictures show inline in terminals that support the kitty graphics protocol
(kitty, Ghostty), sixel (foot, WezTerm) or iTerm2 images. Inside tmux they
show as coloured blocks.

Inside the app, press ? (or F1 while typing) for a list of every key.

FIZZ_NOTIFY=0 turns off desktop notifications for new messages.";

#[tokio::main]
async fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let has = |flag: &str| args.iter().any(|a| a == flag);

    if has("--help") || has("-h") {
        println!("{HELP}");
        return Ok(());
    }
    if has("--version") || has("-V") {
        println!("fizz {}", env!("CARGO_PKG_VERSION"));
        return Ok(());
    }
    if let Some(i) = args.iter().position(|a| a == "--snapshot") {
        return snapshot(args.get(i + 1).map(String::as_str));
    }
    if has("config") {
        return config::edit_settings();
    }
    if has("--demo") {
        return run(None).await;
    }
    if has("setup") {
        let config = config::interactive_setup()?;
        return check(&config).await;
    }

    let config = match config::load()? {
        Some(config) => config,
        None => {
            let config = config::interactive_setup()?;
            check(&config).await?;
            config
        }
    };
    if has("check") {
        return check(&config).await;
    }
    if let Some(unknown) = args.first() {
        eprintln!("Unknown argument: {unknown}\n\n{HELP}");
        std::process::exit(2);
    }
    run(Some(config)).await
}

/// Talk to the server without opening the interface.
async fn check(config: &Config) -> Result<()> {
    match config::settings_problem() {
        Some(problem) => println!(
            "Settings {} is being ignored: {problem}",
            config::settings_path().display()
        ),
        None => println!("Settings {}", config::settings_path().display()),
    }
    println!("Server   {}", config.url);
    let api = Api::new(config)?;
    let info = api.server_info().await.context("couldn't sign in")?;
    println!(
        "         BlueBubbles {} on macOS {}",
        info.server_version.as_deref().unwrap_or("?"),
        info.os_version.as_deref().unwrap_or("?"),
    );
    println!(
        "         Private API {}",
        match (info.private_api, info.helper_connected) {
            (true, true) => "on",
            (true, false) => "enabled, helper not connected",
            _ => "off",
        }
    );
    let recent = api.recent_messages(0, 1).await?;
    println!("Messages {}", if recent.is_empty() { "none found" } else { "ok" });
    match api.contacts().await {
        Ok(contacts) => println!("Contacts {}", contacts.len()),
        Err(e) => println!("Contacts unavailable ({e})"),
    }
    match tokio::time::timeout(Duration::from_secs(10), live::probe(config)).await {
        Ok(Ok(())) => println!("Live     ok"),
        Ok(Err(e)) => println!("Live     unavailable ({e}); falling back to polling"),
        Err(_) => println!("Live     timed out; falling back to polling"),
    }
    println!("Theme    {}", theme::Theme::load().name);
    Ok(())
}

fn enter_extras() {
    let _ = execute!(stdout(), EnableBracketedPaste, EnableFocusChange);
    if config::settings().appearance.mouse {
        let _ = execute!(stdout(), EnableMouseCapture);
    }
    // Lets the terminal tell Shift+Enter apart from Enter, where supported.
    let _ = execute!(
        stdout(),
        PushKeyboardEnhancementFlags(KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES)
    );
}

fn leave_extras() {
    let _ = execute!(
        stdout(),
        PopKeyboardEnhancementFlags,
        DisableBracketedPaste,
        DisableFocusChange,
        DisableMouseCapture
    );
}

async fn run(config: Option<Config>) -> Result<()> {
    let api = config.as_ref().map(Api::new).transpose()?;
    std::thread::spawn(files::prune);

    let mut terminal = ratatui::init();
    // Ask the terminal what kind of pictures it can show before any input
    // is read; fall back to coloured blocks when it cannot say.
    // Inside tmux the question usually goes unanswered, and waiting for it
    // costs two seconds and the first keypress, so don't ask there.
    let in_tmux = std::env::var_os("TMUX").is_some_and(|v| !v.is_empty());
    let picker = if in_tmux {
        Picker::halfblocks()
    } else {
        Picker::from_query_stdio().unwrap_or_else(|_| Picker::halfblocks())
    };
    enter_extras();
    let hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        leave_extras();
        hook(info);
    }));

    let result = event_loop(&mut terminal, api, config, picker).await;

    leave_extras();
    ratatui::restore();
    result
}

async fn event_loop(
    terminal: &mut DefaultTerminal,
    api: Option<Api>,
    config: Option<Config>,
    picker: Picker,
) -> Result<()> {
    let (tx, mut rx) = unbounded_channel();
    let mut app = App::new(api.clone(), tx.clone(), picker);
    match (api, config) {
        (Some(api), Some(config)) => {
            app.start();
            live::start(api, config, tx.clone());
        }
        _ => app.load_demo(),
    }
    theme::watch(tx);

    let mut keys = EventStream::new();
    let mut tick = tokio::time::interval(Duration::from_secs(1));
    while !app.quit {
        terminal.draw(|frame| ui::draw(frame, &mut app))?;
        app.after_draw();
        tokio::select! {
            Some(Ok(event)) = keys.next() => app.on_terminal(event),
            Some(event) = rx.recv() => {
                app.on_event(event);
                // Soak up a burst so it costs one redraw, not many.
                while let Ok(event) = rx.try_recv() {
                    app.on_event(event);
                }
            }
            _ = tick.tick() => app.on_tick(),
        }
    }
    Ok(())
}

/// Render the demo once to plain text: `fizz --snapshot 120x36`.
fn snapshot(size: Option<&str>) -> Result<()> {
    let (w, h) = size
        .and_then(|s| s.split_once('x'))
        .and_then(|(w, h)| Some((w.parse().ok()?, h.parse().ok()?)))
        .unwrap_or((120u16, 36u16));
    let (tx, _rx) = unbounded_channel();
    let mut app = App::new(None, tx, Picker::halfblocks());
    app.load_demo();
    let mut terminal = Terminal::new(TestBackend::new(w, h))?;
    terminal.draw(|frame| ui::draw(frame, &mut app))?;
    let buffer = terminal.backend().buffer();
    for y in 0..h {
        let row: String = (0..w)
            .map(|x| buffer.cell((x, y)).map_or(" ", |c| c.symbol()))
            .collect();
        println!("{}", row.trim_end());
    }
    Ok(())
}
