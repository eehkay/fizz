//! Fizz: a terminal client for iMessage through a BlueBubbles server.

mod api;
mod app;
mod bridge;
mod codes;
mod config;
mod demo;
mod emoji;
mod files;
mod images;
mod index;
mod input;
mod live;
mod metrics;
mod model;
mod setup;
mod storage;
mod store;
mod theme;
mod update;
mod ui;
mod widget;

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
  fizz update     Install the newest version (--check only looks)
  fizz storage    Show what Fizz keeps on disk and each store's limit
  fizz index      Show the message-text search index (add: delete, to
                  remove it and turn it off)
  fizz sound      Play the notification sound
  fizz widget     Put Fizz in the Omarchy bar (install, or remove)
  fizz --demo     Try the interface with made-up conversations

The server address and password live in ~/.config/fizz/env (BB_URL and
BB_PASSWORD); environment variables of the same names override the file.
Everything else (notifications, colours, picture sizes, clock, and so on)
is in ~/.config/fizz/config.toml, which lists every option with its default.

Colours follow the current Omarchy theme when there is one, and your
terminal's own colours otherwise. FIZZ_THEME=ansi forces the latter, and
FIZZ_THEME=/path/to/colors.toml uses a specific palette.

Fizz works best in kitty or Ghostty. Pictures show inline in terminals that
support the kitty graphics protocol (kitty, Ghostty), sixel (foot, WezTerm)
or iTerm2 images; in other terminals, and inside tmux, they show as coloured
blocks. Everything else works anywhere.

Inside the app, press ? (or F1 while typing) for a list of every key.

FIZZ_NOTIFY=0 turns off desktop notifications for new messages.";

/// The first argument from `from` on that is a word rather than a flag,
/// and where it is. Flags can come anywhere, but what Fizz is being asked
/// to do is only ever the first word; the words after it belong to it.
/// `fizz index count update` counts messages that say "update", and must
/// never be taken for an update (or `... count delete` for a deletion).
fn first_word(args: &[String], from: usize) -> Option<(usize, &str)> {
    args.iter()
        .enumerate()
        .skip(from)
        .map(|(i, arg)| (i, arg.as_str()))
        .find(|(_, arg)| !arg.starts_with('-'))
}

#[tokio::main]
async fn main() -> Result<()> {
    // For the commands that print and exit: if whatever is reading the
    // output stops early (`fizz check | head`), end quietly as other
    // tools do, rather than with an error about a broken pipe.
    quiet_on_closed_pipe(true);
    let args: Vec<String> = std::env::args().skip(1).collect();
    let has = |flag: &str| args.iter().any(|a| a == flag);
    let command = first_word(&args, 0).map(|(_, word)| word);
    let is = |name: &str| command == Some(name);

    if has("--help") || has("-h") {
        println!("{HELP}");
        return Ok(());
    }
    if has("--version") || has("-V") {
        let commit = update::built_commit();
        let release = update::built_release();
        let detail = match (release.is_empty(), commit.is_empty()) {
            (false, false) => format!(" ({release} download, {})", update::short(&commit)),
            (false, true) => format!(" ({release} download)"),
            (true, false) => format!(" ({})", update::short(&commit)),
            (true, true) => String::new(),
        };
        println!("fizz {}{detail}", env!("CARGO_PKG_VERSION"));
        return Ok(());
    }
    if let Some(i) = args.iter().position(|a| a == "--snapshot") {
        return snapshot(args.get(i + 1).map(String::as_str));
    }
    if is("config") {
        return config::edit_settings();
    }
    if is("storage") {
        // What each store is using against its limit. Trim first, so the
        // figures are what the limits leave.
        storage::trim();
        println!("{:<12} {:>9}  {:>7}  {}", "", "on disk", "items", "limit");
        let mut total = 0;
        for line in storage::report() {
            total += line.bytes;
            println!(
                "{:<12} {:>6.1} MB  {:>7}  {}",
                line.name,
                line.bytes as f64 / 1048576.0,
                line.items,
                line.limit
            );
        }
        println!("{:<12} {:>6.1} MB", "Total", total as f64 / 1048576.0);
        println!();
        for line in storage::report() {
            println!("{:<12} {}", line.name, line.place.display());
        }
        println!("\nLimits are in the settings: fizz config  (see [search] and [media])");
        return Ok(());
    }
    if is("update") {
        return update::update(has("--check")).await;
    }
    if is("sound") {
        // Play the notification sound, to hear it or check it works.
        let chosen = config::settings().notifications.sound.clone();
        let (what, choice) = match chosen.trim() {
            "" | "off" => ("Sound is off in the settings; this is the built-in pop.", "pop"),
            other => ("Playing the sound from your settings.", other),
        };
        println!("{what}");
        if !files::play_sound(choice) {
            println!("Couldn't play it: no audio player found (pw-play, paplay, afplay, ffplay or aplay), or the file is missing.");
        }
        // Give the player a moment before this process exits.
        std::thread::sleep(Duration::from_millis(600));
        return Ok(());
    }
    if is("widget") {
        return widget::command(&args);
    }
    if is("bridge") {
        // For a status-bar widget: no interface, and nothing asked of whoever
        // started it, so missing server details are an error, not a prompt.
        let config = match has("--demo") {
            true => None,
            false => Some(config::load()?.context("no server details yet: run `fizz setup` first")?),
        };
        return bridge::run(config).await;
    }
    if has("--demo") {
        return run(None).await;
    }
    if is("setup") {
        let done = setup::run().await?;
        if done.verified {
            check(&done.config).await?;
            setup::next_steps();
        }
        return Ok(());
    }

    let config = match config::load()? {
        Some(config) => config,
        // First run: ask, then carry on into the app.
        None => setup::run().await?.config,
    };
    if is("check") {
        return check(&config).await;
    }
    if is("index") {
        return index_command(&args, &config).await;
    }
    if let Some(unknown) = command.or(args.first().map(String::as_str)) {
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
    let recent = api.recent_messages(None, None, 1).await?;
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
    println!("Pictures {}", picture_support());
    Ok(())
}

/// `fizz index`: look at, build or remove the local copy of message text
/// that searching by words uses.
async fn index_command(args: &[String], config: &Config) -> Result<()> {
    let has = |word: &str| args.iter().any(|a| a == word);
    // What to do is the word after "index"; anything further on is that
    // command's own.
    let index_at = first_word(args, 0).map_or(0, |(i, _)| i);
    let action = first_word(args, index_at + 1);
    let is = |name: &str| action.is_some_and(|(_, word)| word == name);
    if is("delete") {
        index::delete()?;
        println!("The search index has been removed and turned off.");
        return Ok(());
    }
    if is("build") {
        // Download in the foreground, optionally only so many pages.
        let pages = args
            .iter()
            .position(|a| a == "--pages")
            .and_then(|i| args.get(i + 1))
            .and_then(|n| n.parse::<usize>().ok());
        index::enable()?;
        let api = Api::new(config)?;
        let (tx, mut rx) = unbounded_channel();
        let (_live_tx, mut live_rx) = unbounded_channel();
        let work = tokio::spawn(async move {
            index::run(api, tx, &mut live_rx, pages.or(Some(usize::MAX))).await
        });
        while let Some(event) = rx.recv().await {
            if let app::AppEvent::Indexing(p) = event {
                println!(
                    "{:>3}%  {} messages stored",
                    p.percent().map_or("  ?".into(), |n| n.to_string()),
                    p.stored
                );
            }
        }
        return work.await?;
    }
    if let Some((i, "count")) = action {
        // How many messages contain these words, and how long finding out took.
        let query = args[i + 1..].join(" ");
        let conn = store::open()?;
        let started = std::time::Instant::now();
        let matching = store::count_matching(&conn, &query)?;
        let counted = started.elapsed();
        // What the search screen does: the newest eighty, with their text.
        let started = std::time::Instant::now();
        let shown = store::search(&conn, &query, 80)?.len();
        let fetched = started.elapsed();
        // And which conversations they are in.
        let started = std::time::Instant::now();
        let conversations = store::mentions(&conn, &query)?.len();
        println!(
            "{matching} messages in {conversations} conversations (counted in {} ms; \
             the newest {shown} fetched in {} ms; grouped by conversation in {} ms)",
            counted.as_millis(),
            fetched.as_millis(),
            started.elapsed().as_millis()
        );
        return Ok(());
    }
    if is("upgrade") {
        // Bring an index kept as text files into the database. Normally
        // this happens by itself the next time Fizz starts.
        let started = std::time::Instant::now();
        match index::upgrade(has("--keep"))? {
            Some(found) => println!(
                "Read {} messages: {} new, {} missing afterwards ({} ms)",
                found.read,
                found.added,
                found.missing,
                started.elapsed().as_millis()
            ),
            None => println!("There is no old index to bring across."),
        }
        return Ok(());
    }
    if !index::enabled() {
        println!("Message text is not being stored, so only names can be searched.");
        println!("Turn it on inside Fizz: press / to search, then Ctrl+B.");
        return Ok(());
    }
    let p = index::Progress::load();
    let stored = store::open().map(|conn| store::message_count(&conn)).unwrap_or(p.stored);
    println!("Stored    {stored} messages");
    println!(
        "Progress  {}",
        match (p.complete, p.percent()) {
            (true, _) => "complete".to_string(),
            (false, Some(n)) => format!("{n}% of the server's {} messages", p.total.unwrap_or(0)),
            (false, None) => "just started".to_string(),
        }
    );
    println!(
        "On disk   {:.1} MB in {}",
        store::size_on_disk() as f64 / 1e6,
        store::path().display()
    );
    if index::old_dir().join("messages.tsv").exists() {
        println!("          (an older text index is still present and will be brought across)");
    }
    println!("\nRemove it and turn it off with: fizz index delete");
    Ok(())
}

/// What this terminal can do with pictures, in a sentence.
fn picture_support() -> String {
    use ratatui_image::picker::ProtocolType;
    use std::io::IsTerminal;
    const BEST: &str = "Fizz works best in kitty or Ghostty";
    if std::env::var_os("TMUX").is_some_and(|v| !v.is_empty()) {
        return format!("coloured blocks only inside tmux. {BEST}, outside tmux");
    }
    if !std::io::stdin().is_terminal() || !std::io::stdout().is_terminal() {
        return "can't tell (not run in a terminal)".into();
    }
    match Picker::from_query_stdio().map(|p| p.protocol_type()) {
        Ok(ProtocolType::Kitty) => "shown as real pictures (kitty graphics)".into(),
        Ok(ProtocolType::Sixel) => "shown as real pictures (sixel)".into(),
        Ok(ProtocolType::Iterm2) => "shown as real pictures (iTerm2)".into(),
        _ => format!("coloured blocks only in this terminal. {BEST}"),
    }
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

/// Whether a write to a closed pipe should end the program silently
/// (right for print-and-exit commands) or be reported as an ordinary
/// error (right for the app, which must put the terminal back first).
fn quiet_on_closed_pipe(quiet: bool) {
    #[cfg(unix)]
    // Safe: changing how one signal is handled, before any other threads
    // depend on it.
    unsafe {
        libc::signal(libc::SIGPIPE, if quiet { libc::SIG_DFL } else { libc::SIG_IGN });
    }
    #[cfg(not(unix))]
    let _ = quiet;
}

async fn run(config: Option<Config>) -> Result<()> {
    quiet_on_closed_pipe(false);
    let api = config.as_ref().map(Api::new).transpose()?;
    std::thread::spawn(storage::trim);

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
    let app_is_live = api.is_some() || std::env::var("FIZZ_COMMIT_OVERRIDE").is_ok();
    match (api, config) {
        (Some(api), Some(config)) => {
            app.start();
            live::start(api, config, tx.clone());
        }
        _ => app.load_demo(),
    }
    app.hint_about_terminal();
    theme::watch(tx.clone());
    if app_is_live {
        update::watch(tx);
    }

    let mut keys = EventStream::new();
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
            _ = tokio::time::sleep(app.tick_rate()) => app.on_tick(),
        }
    }
    app.remember();
    // Do not leave "typing…" showing on the other side after we have gone.
    if let Some(stop) = app.stop_typing() {
        let _ = tokio::time::timeout(Duration::from_secs(2), stop).await;
    }
    Ok(())
}

/// Render the demo once to plain text: `fizz --snapshot 120x36`. Add
/// `--splash` to render the loading screen instead, or `--search <words>`
/// for the search layer with those words typed.
fn snapshot(size: Option<&str>) -> Result<()> {
    let (w, h) = size
        .and_then(|s| s.split_once('x'))
        .and_then(|(w, h)| Some((w.parse().ok()?, h.parse().ok()?)))
        .unwrap_or((120u16, 36u16));
    let (tx, _rx) = unbounded_channel();
    let mut app = App::new(None, tx, Picker::halfblocks());
    app.load_demo();
    app.splash_until = std::env::args()
        .any(|a| a == "--splash")
        .then(|| std::time::Instant::now() + Duration::from_secs(3600));
    app.frame = 23;
    let args: Vec<String> = std::env::args().collect();
    if let Some(i) = args.iter().position(|a| a == "--search") {
        app.search_for(&args[i + 1..].join(" "));
    }
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

#[cfg(test)]
mod tests {
    use super::first_word;

    #[test]
    fn only_the_first_word_is_a_command() {
        let args = |words: &[&str]| words.iter().map(|w| w.to_string()).collect::<Vec<_>>();
        let command = |words: &[&str]| first_word(&args(words), 0).map(|(_, w)| w.to_string());
        assert_eq!(command(&["index", "count", "update"]).as_deref(), Some("index"));
        assert_eq!(command(&["--demo", "bridge"]).as_deref(), Some("bridge"));
        assert_eq!(command(&["update", "--check"]).as_deref(), Some("update"));
        assert_eq!(command(&["--demo"]), None);
        assert_eq!(command(&[]), None);
        // And the same one level down: what `index` is asked to do.
        let counting = args(&["index", "count", "delete", "everything"]);
        assert_eq!(first_word(&counting, 1), Some((1, "count")));
        assert_eq!(first_word(&args(&["index", "--keep", "upgrade"]), 1), Some((2, "upgrade")));
        assert_eq!(first_word(&args(&["index"]), 1), None);
    }
}
