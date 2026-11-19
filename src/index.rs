//! A local copy of message text, so messages can be searched by what was
//! said. The server cannot do this itself, so Fizz downloads the text once
//! (newest first, in the background), keeps it up to date, and searches it
//! here. Nothing is stored unless the user turns this on.
//!
//! The store is one line per message in a plain file. Searching reads the
//! whole file and scans it, which for half a million messages takes a few
//! hundredths of a second and needs no database.

use aho_corasick::AhoCorasick;
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::io::Write;
use std::path::PathBuf;
use std::time::Duration;
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};

use crate::api::Api;
use crate::app::AppEvent;
use crate::model::Message;

/// Messages fetched per request while downloading history. Kept modest,
/// with a pause between requests, because the server answers one thing at
/// a time: big pages would make the app itself feel slow while this runs.
const PAGE: usize = 300;
const PAUSE: Duration = Duration::from_millis(450);

pub fn dir() -> PathBuf {
    dirs::data_local_dir()
        .unwrap_or_else(std::env::temp_dir)
        .join("fizz")
        .join("index")
}

fn messages_path() -> PathBuf {
    dir().join("messages.tsv")
}

fn chats_path() -> PathBuf {
    dir().join("chats.txt")
}

fn state_path() -> PathBuf {
    dir().join("state.json")
}

/// The user has said yes to keeping message text on this computer.
pub fn enabled() -> bool {
    dir().join("enabled").exists()
}

pub fn enable() -> Result<()> {
    let dir = dir();
    std::fs::create_dir_all(&dir)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700));
    }
    std::fs::write(dir.join("enabled"), "")?;
    Ok(())
}

/// Remove the stored message text and turn the feature off.
pub fn delete() -> Result<()> {
    match std::fs::remove_dir_all(dir()) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e).context("couldn't remove the search index"),
    }
}

/// How far the download has got.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct Progress {
    /// Dates (milliseconds) of the oldest and newest messages stored.
    pub oldest: i64,
    pub newest: i64,
    /// Whether history has been followed all the way back.
    pub complete: bool,
    pub stored: u64,
    /// How many of the server's messages have been gone through, including
    /// those not worth storing (tapbacks, pictures with no text).
    #[serde(default)]
    pub scanned: u64,
    /// How many messages the server says exist, once known.
    pub total: Option<u64>,
}

impl Progress {
    pub fn load() -> Self {
        std::fs::read(state_path())
            .ok()
            .and_then(|bytes| serde_json::from_slice(&bytes).ok())
            .unwrap_or_default()
    }

    fn save(&self) {
        if let Ok(json) = serde_json::to_vec(self) {
            let tmp = state_path().with_extension("tmp");
            if std::fs::write(&tmp, json).is_ok() {
                let _ = std::fs::rename(tmp, state_path());
            }
        }
    }

    pub fn percent(&self) -> Option<u8> {
        if self.complete {
            return Some(100);
        }
        let total = self.total.filter(|t| *t > 0)?;
        Some(((self.scanned.min(total) * 100) / total).min(99) as u8)
    }
}

fn escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\t' => out.push_str("\\t"),
            '\r' => {}
            other => out.push(other),
        }
    }
    out
}

fn unescape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('n') => out.push('\n'),
            Some('t') => out.push('\t'),
            Some(other) => out.push(other),
            None => {}
        }
    }
    out
}

/// Appends messages to the store, numbering chats so each line stays short.
struct Writer {
    file: std::fs::File,
    chats: HashMap<String, usize>,
    chats_file: std::fs::File,
}

impl Writer {
    fn open() -> Result<Self> {
        std::fs::create_dir_all(dir())?;
        let open = |path: PathBuf| {
            let mut options = std::fs::OpenOptions::new();
            options.create(true).append(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                options.mode(0o600);
            }
            options.open(path)
        };
        let known = std::fs::read_to_string(chats_path()).unwrap_or_default();
        let chats = known
            .lines()
            .enumerate()
            .map(|(i, guid)| (guid.to_string(), i))
            .collect();
        Ok(Writer {
            file: open(messages_path())?,
            chats,
            chats_file: open(chats_path())?,
        })
    }

    fn chat_number(&mut self, guid: &str) -> Result<usize> {
        if let Some(n) = self.chats.get(guid) {
            return Ok(*n);
        }
        let n = self.chats.len();
        writeln!(self.chats_file, "{guid}")?;
        self.chats.insert(guid.to_string(), n);
        Ok(n)
    }

    /// Store what is worth searching; returns how many lines were written.
    fn add(&mut self, messages: &[Message]) -> Result<u64> {
        let mut out = String::new();
        let mut written = 0;
        for m in messages {
            // Tapbacks ("Loved …") and events are not things people said.
            if m.reaction().is_some() || m.item_type != 0 {
                continue;
            }
            let text = m.body();
            let Some(chat) = m.chats.first() else { continue };
            if text.is_empty() || chat.guid.is_empty() {
                continue;
            }
            let chat = self.chat_number(&chat.guid)?;
            let who = match (&m.handle, m.is_from_me) {
                (_, true) => "",
                (Some(h), false) => h.address.as_str(),
                (None, false) => "",
            };
            out.push_str(&format!(
                "{}\t{chat}\t{}\t{}\t{}\t{}\n",
                m.date_created,
                if m.is_from_me { 'm' } else { 't' },
                escape(who),
                escape(&m.guid),
                escape(&text),
            ));
            written += 1;
        }
        self.file.write_all(out.as_bytes())?;
        Ok(written)
    }
}

/// Keep the store complete: catch up on anything newer than what is held,
/// follow history back to the beginning, and add messages as they arrive.
pub async fn run(
    api: Api,
    tx: UnboundedSender<AppEvent>,
    mut live: UnboundedReceiver<Message>,
    max_pages: Option<usize>,
) -> Result<()> {
    let mut writer = Writer::open()?;
    let mut progress = Progress::load();
    let report = |p: &Progress| {
        let _ = tx.send(AppEvent::Indexing(p.clone()));
    };
    report(&progress);
    let mut pages = 0;

    // Anything that arrived while Fizz was not running.
    while progress.newest > 0 {
        let page = api.history(None, Some(progress.newest), PAGE).await?;
        let got = page.len();
        if let Some(latest) = page.iter().map(|m| m.date_created).max() {
            // The boundary message comes back again; do not count it twice.
            let fresh: Vec<Message> = page
                .into_iter()
                .filter(|m| m.date_created > progress.newest)
                .collect();
            progress.stored += writer.add(&fresh)?;
            progress.newest = progress.newest.max(latest);
        }
        progress.save();
        report(&progress);
        if got < PAGE {
            break;
        }
    }

    // Then history, newest first, so recent messages are searchable early.
    // With a limit in the settings, stop once that far back.
    let years = i64::from(crate::config::settings().search.years);
    let cutoff = (years > 0)
        .then(|| chrono::Utc::now().timestamp_millis() - years * 365 * 24 * 3600 * 1000);
    while !progress.complete {
        if cutoff.is_some_and(|cutoff| progress.oldest > 0 && progress.oldest < cutoff) {
            progress.complete = true;
            progress.save();
            report(&progress);
            break;
        }
        if max_pages.is_some_and(|max| pages >= max) {
            break;
        }
        let before = (progress.oldest > 0).then_some(progress.oldest);
        let (page, total) = api.history_with_total(before, PAGE).await?;
        pages += 1;
        if progress.total.is_none() || before.is_none() {
            progress.total = total;
        }
        if page.is_empty() {
            progress.complete = true;
        } else {
            progress.scanned += page.len() as u64;
            progress.stored += writer.add(&page)?;
            let oldest = page.iter().map(|m| m.date_created).min().unwrap_or(0);
            let newest = page.iter().map(|m| m.date_created).max().unwrap_or(0);
            progress.oldest = if progress.oldest == 0 { oldest } else { progress.oldest.min(oldest) };
            progress.newest = progress.newest.max(newest);
            if page.len() < PAGE {
                progress.complete = true;
            }
        }
        progress.save();
        report(&progress);
        // Leave the server room to serve the app itself.
        tokio::time::sleep(PAUSE).await;
    }
    if max_pages.is_some() {
        return Ok(());
    }

    // From here on, store messages as the app hears about them.
    while let Some(message) = live.recv().await {
        if message.date_created > progress.newest {
            progress.stored += writer.add(std::slice::from_ref(&message))?;
            progress.newest = message.date_created;
            progress.save();
        }
    }
    Ok(())
}

/// One message found by a search.
#[derive(Debug, Clone, PartialEq)]
pub struct Hit {
    pub chat: String,
    pub guid: String,
    pub date: i64,
    pub from_me: bool,
    pub sender: String,
    pub text: String,
}

/// Find messages containing every word of `query`, newest first. Case is
/// ignored for plain letters. Blocking: call it off the UI thread.
pub fn search(query: &str, limit: usize) -> Result<Vec<Hit>> {
    let words: Vec<String> = query
        .split_whitespace()
        .map(|w| w.to_lowercase())
        .filter(|w| !w.is_empty())
        .collect();
    let Some(longest) = words.iter().max_by_key(|w| w.len()) else {
        return Ok(Vec::new());
    };
    let Ok(data) = std::fs::read(messages_path()) else {
        return Ok(Vec::new());
    };
    let chats: Vec<String> = std::fs::read_to_string(chats_path())
        .unwrap_or_default()
        .lines()
        .map(str::to_string)
        .collect();
    let finder = AhoCorasick::builder()
        .ascii_case_insensitive(true)
        .build([longest.as_str()])
        .context("couldn't prepare the search")?;

    // Scan for the longest word, which rules out the most; check the rest
    // only on lines where it was found.
    let mut found: Vec<(i64, usize, usize)> = Vec::new();
    let mut last_line_end = 0;
    for hit in finder.find_iter(&data) {
        if hit.start() < last_line_end {
            continue; // Another match on a line already taken.
        }
        let start = data[..hit.start()]
            .iter()
            .rposition(|b| *b == b'\n')
            .map_or(0, |i| i + 1);
        let Some(end) = data[hit.start()..].iter().position(|b| *b == b'\n') else {
            break; // A line still being written.
        };
        let end = hit.start() + end;
        last_line_end = end;
        // The match has to be in the text, which is the last field.
        let line = &data[start..end];
        let Some(text_at) = line.iter().rposition(|b| *b == b'\t') else {
            continue;
        };
        if hit.start() < start + text_at + 1 {
            continue;
        }
        if words.len() > 1 {
            let text = String::from_utf8_lossy(&line[text_at + 1..]).to_lowercase();
            if !words.iter().all(|w| text.contains(w.as_str())) {
                continue;
            }
        }
        let date = line
            .iter()
            .position(|b| *b == b'\t')
            .and_then(|i| std::str::from_utf8(&line[..i]).ok())
            .and_then(|d| d.parse::<i64>().ok())
            .unwrap_or(0);
        found.push((date, start, end));
    }
    found.sort_by_key(|(date, ..)| std::cmp::Reverse(*date));

    let mut hits: Vec<Hit> = Vec::new();
    for (date, start, end) in found {
        if hits.len() >= limit {
            break;
        }
        let line = String::from_utf8_lossy(&data[start..end]);
        let mut fields = line.splitn(6, '\t');
        let (_, chat, who, sender, guid, text) = (
            fields.next(),
            fields.next().and_then(|c| c.parse::<usize>().ok()),
            fields.next().unwrap_or(""),
            fields.next().unwrap_or(""),
            fields.next().unwrap_or(""),
            fields.next().unwrap_or(""),
        );
        let Some(chat) = chat.and_then(|n| chats.get(n)) else {
            continue;
        };
        let guid = unescape(guid);
        // The same message can be stored twice at a page boundary.
        if hits.iter().any(|h| h.guid == guid) {
            continue;
        }
        hits.push(Hit {
            chat: chat.clone(),
            guid,
            date,
            from_me: who == "m",
            sender: unescape(sender),
            text: unescape(text),
        });
    }
    Ok(hits)
}

/// Size of the store on disk, in bytes.
pub fn size_on_disk() -> u64 {
    crate::metrics::folder_size(&dir())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_survives_the_file_format() {
        for text in ["plain", "tab\there", "two\nlines", "back\\slash", "mix\\n\t\n\\"] {
            assert_eq!(unescape(&escape(text)), text);
            assert!(!escape(text).contains('\n') && !escape(text).contains('\t'));
        }
    }

    #[test]
    fn progress_reports_sensible_percentages() {
        let mut p = Progress { scanned: 250, total: Some(1000), ..Progress::default() };
        assert_eq!(p.percent(), Some(25));
        p.scanned = 5000; // More seen than the server counted: still not "done".
        assert_eq!(p.percent(), Some(99));
        p.complete = true;
        assert_eq!(p.percent(), Some(100));
        assert_eq!(Progress::default().percent(), None);
    }
}
