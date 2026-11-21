//! `fizz setup`: ask for the server's address and password, test them, and
//! save them to `~/.config/fizz/env`.
//!
//! The password shows as dots while it is typed, and nothing is saved
//! until the server has accepted it (or the user says to save regardless).

use crossterm::event::{Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use std::io::{self, BufRead, IsTerminal, Write};
use std::time::Duration;

use anyhow::{Result, bail};

use crate::api::{self, Api};
use crate::config::{self, Config};
use crate::model::ServerInfo;

/// What `run` came back with: the details, and whether the server took them.
pub struct Outcome {
    pub config: Config,
    pub verified: bool,
}

/// Stop, having saved nothing.
fn quit(code: i32) -> ! {
    println!("\nNothing was saved. Run `fizz setup` when you are ready.");
    std::process::exit(code)
}

/// Read one line as typed. `None` when there is no more input.
fn ask(label: &str) -> Result<Option<String>> {
    print!("{label}");
    io::stdout().flush()?;
    let mut line = String::new();
    if io::stdin().lock().read_line(&mut line)? == 0 {
        return Ok(None);
    }
    Ok(Some(line.trim().to_string()))
}

/// What one key does to a password being typed.
#[derive(Debug, PartialEq)]
enum Step {
    Added,
    Removed,
    Cleared(usize),
    Done,
    Cancel,
    Nothing,
}

fn press(typed: &mut String, key: KeyEvent) -> Step {
    if key.kind == KeyEventKind::Release {
        return Step::Nothing;
    }
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    match key.code {
        KeyCode::Enter => Step::Done,
        KeyCode::Char('c' | 'd') if ctrl => Step::Cancel,
        KeyCode::Char('u') if ctrl => {
            let count = typed.chars().count();
            typed.clear();
            Step::Cleared(count)
        }
        KeyCode::Backspace | KeyCode::Char('h') if ctrl || key.code == KeyCode::Backspace => {
            match typed.pop() {
                Some(_) => Step::Removed,
                None => Step::Nothing,
            }
        }
        KeyCode::Char(c) if !ctrl && !key.modifiers.contains(KeyModifiers::ALT) => {
            typed.push(c);
            Step::Added
        }
        _ => Step::Nothing,
    }
}

/// Puts the terminal back to normal line input when dropped.
struct KeyByKey;

impl Drop for KeyByKey {
    fn drop(&mut self) {
        let _ = crossterm::terminal::disable_raw_mode();
    }
}

/// Read a line without showing it: each character appears as a dot.
/// Where there is no terminal to do that on (input piped in), it is read
/// as an ordinary line.
fn ask_hidden(label: &str) -> Result<Option<String>> {
    if !io::stdin().is_terminal() || !io::stdout().is_terminal() {
        return ask(label);
    }
    let mut out = io::stdout();
    write!(out, "{label}")?;
    out.flush()?;
    let mut typed = String::new();
    crossterm::terminal::enable_raw_mode()?;
    let reading = KeyByKey;
    let cancelled = loop {
        let Event::Key(key) = crossterm::event::read()? else {
            continue;
        };
        match press(&mut typed, key) {
            Step::Added => write!(out, "•")?,
            Step::Removed => write!(out, "\x08 \x08")?,
            Step::Cleared(count) => write!(out, "{}", "\x08 \x08".repeat(count))?,
            Step::Done => break false,
            Step::Cancel => break true,
            Step::Nothing => {}
        }
        out.flush()?;
    };
    drop(reading);
    println!();
    if cancelled {
        quit(130);
    }
    Ok(Some(typed))
}

/// Say what a failed test means and what to try, in place of the bare error.
fn explain(problem: &str) -> String {
    if problem.contains(api::UNREACHABLE) {
        "Nothing answered at that address. Check the address and port, that \
         BlueBubbles Server is running on the Mac, and that this computer can \
         reach it (same network, or your VPN or proxy is on)."
            .into()
    } else if problem.contains("HTTP 401") || problem.contains("HTTP 403") {
        "The server answered but did not accept the password. It is the one \
         under BlueBubbles Server > Settings on the Mac."
            .into()
    } else if problem.contains("unreadable reply") || problem.contains("HTTP 404") {
        "Something answered at that address, but not a BlueBubbles server. \
         Check the port: BlueBubbles shows its address on the Mac."
            .into()
    } else if problem.contains("too long") {
        "The server did not answer in time. Check that the Mac is awake and \
         reachable from this computer."
            .into()
    } else {
        format!("The server could not be used: {problem}.")
    }
}

/// Where the details were saved, written the short way (`~/…`).
fn saved_at() -> String {
    let path = config::config_path();
    match dirs::home_dir().and_then(|home| path.strip_prefix(home).ok().map(|rest| rest.to_owned())) {
        Some(rest) => format!("~/{}", rest.display()),
        None => path.display().to_string(),
    }
}

async fn test(config: &Config) -> Result<ServerInfo> {
    let api = Api::new(config)?;
    match tokio::time::timeout(Duration::from_secs(15), api.server_info()).await {
        Ok(result) => result,
        Err(_) => bail!("the server took too long to answer"),
    }
}

/// Wrap text to a width with a hanging indent, for the explanations.
fn indented(text: &str) -> String {
    textwrap::fill(
        text,
        textwrap::Options::new(72).initial_indent("  ").subsequent_indent("  "),
    )
}

/// Ask for the server details, test them, and save them. Returns once
/// something has been saved; quitting part-way ends the program.
pub async fn run() -> Result<Outcome> {
    let existing = config::load()?;
    let at_terminal = io::stdin().is_terminal();
    let mut url = existing.as_ref().map(|c| c.url.clone()).unwrap_or_default();
    let mut password = existing.map(|c| c.password).unwrap_or_default();

    println!("Connect Fizz to your BlueBubbles server\n");
    println!("  Address   whatever reaches the Mac running BlueBubbles Server,");
    println!("            such as http://my-mac:1234 or your proxy URL");
    println!("  Password  the one under BlueBubbles Server > Settings\n");
    if !url.is_empty() {
        println!("  Press Enter at either to keep what is saved now.\n");
    }

    loop {
        let label = match url.is_empty() {
            true => "Server address:  ".to_string(),
            false => format!("Server address [{url}]:  "),
        };
        match ask(&label)? {
            None => quit(1),
            Some(typed) if !typed.is_empty() => url = config::clean_url(&typed),
            Some(_) => {}
        }
        let label = match password.is_empty() {
            true => "Server password: ",
            false => "Server password [Enter keeps it]: ",
        };
        match ask_hidden(label)? {
            None => quit(1),
            Some(typed) if !typed.is_empty() => password = typed,
            Some(_) => {}
        }
        if url.is_empty() || password.is_empty() {
            if !at_terminal {
                bail!("both the address and the password are required");
            }
            println!("\nBoth are needed.\n");
            continue;
        }

        let config = Config {
            url: url.clone(),
            password: password.clone(),
        };
        print!("\nChecking {} … ", config.url);
        io::stdout().flush()?;
        let problem = match test(&config).await {
            Ok(info) => {
                println!(
                    "connected (BlueBubbles {} on macOS {}).",
                    info.server_version.as_deref().unwrap_or("?"),
                    info.os_version.as_deref().unwrap_or("?"),
                );
                config::save(&config)?;
                println!("Saved to {}, readable only by you.\n", saved_at());
                return Ok(Outcome { config, verified: true });
            }
            Err(problem) => format!("{problem:#}"),
        };
        println!("no.\n\n{}\n", indented(&explain(&problem)));
        if !at_terminal {
            bail!("the server details were not saved");
        }
        match ask("Enter to try again, s to save these anyway, q to quit: ")?.as_deref() {
            None | Some("q" | "Q") => quit(1),
            Some("s" | "S") => {
                config::save(&config)?;
                println!("\nSaved to {} without a working connection.", saved_at());
                println!("`fizz check` tests it again whenever you like.\n");
                return Ok(Outcome { config, verified: false });
            }
            Some(_) => println!(),
        }
    }
}

/// What to do next, once setup has worked.
pub fn next_steps() {
    println!("\nYou're set. Start it with:  fizz");
    let on_omarchy = dirs::config_dir().is_some_and(|dir| dir.join("omarchy").is_dir());
    if on_omarchy {
        println!("Put it in the Omarchy bar:  fizz widget install");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn ctrl(c: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL)
    }

    #[test]
    fn a_password_is_typed_edited_and_entered() {
        let mut typed = String::new();
        for c in "pä$s w".chars() {
            assert_eq!(press(&mut typed, key(KeyCode::Char(c))), Step::Added);
        }
        assert_eq!(press(&mut typed, key(KeyCode::Backspace)), Step::Removed);
        assert_eq!(typed, "pä$s ");
        // Arrow keys and the like neither add nor show anything.
        assert_eq!(press(&mut typed, key(KeyCode::Left)), Step::Nothing);
        assert_eq!(press(&mut typed, ctrl('a')), Step::Nothing);
        assert_eq!(typed, "pä$s ");
        assert_eq!(press(&mut typed, key(KeyCode::Enter)), Step::Done);
        assert_eq!(typed, "pä$s ", "spaces are part of a password");
    }

    #[test]
    fn clearing_and_cancelling() {
        let mut typed = String::from("abc");
        assert_eq!(press(&mut typed, ctrl('u')), Step::Cleared(3));
        assert_eq!(press(&mut typed, key(KeyCode::Backspace)), Step::Nothing);
        assert_eq!(press(&mut typed, ctrl('c')), Step::Cancel);
        assert!(typed.is_empty());
    }

    #[test]
    fn failures_are_explained() {
        assert!(explain(api::UNREACHABLE).contains("Nothing answered"));
        assert!(explain("Unauthorized (HTTP 401)").contains("password"));
        assert!(explain("the server sent an unreadable reply (HTTP 200 OK)").contains("not a BlueBubbles"));
        assert!(explain("something odd").contains("something odd"));
    }
}
