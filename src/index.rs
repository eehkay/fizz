//! Keeping message text on this computer so messages can be searched by
//! what was said. The server cannot do this itself, so Fizz downloads the
//! text once (newest first, in the background), keeps it up to date, and
//! searches it in its own database (see `store.rs`). Nothing is stored
//! unless the user turns this on.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU8, Ordering};
use std::time::Duration;
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};

use crate::api::Api;
use crate::app::AppEvent;
use crate::model::Message;
use crate::store;

pub use crate::store::{Hit, Mention};

/// Messages fetched per request while downloading history. Kept modest,
/// with a pause between requests, because the server answers one thing at
/// a time: big pages would make the app itself feel slow while this runs.
const PAGE: usize = 300;
const PAUSE: Duration = Duration::from_millis(450);

const ENABLED_KEY: &str = "search_text";
const PROGRESS_KEY: &str = "search_progress";

/// Where the index lived before the database: a folder of text files.
/// Still looked for, so an existing index is brought across.
pub fn old_dir() -> PathBuf {
    store::folder().join("index")
}

/// Whether the user has said yes to keeping message text. Asked on every
/// frame, so the answer is remembered rather than read from disk each time
/// (0 = not looked up yet, 1 = off, 2 = on).
static ENABLED: AtomicU8 = AtomicU8::new(0);

pub fn enabled() -> bool {
    match ENABLED.load(Ordering::Relaxed) {
        0 => {
            let on = old_dir().join("enabled").exists()
                || (store::path().exists()
                    && store::open()
                        .ok()
                        .and_then(|conn| store::state(&conn, ENABLED_KEY))
                        .is_some_and(|v| v == "1"));
            ENABLED.store(if on { 2 } else { 1 }, Ordering::Relaxed);
            on
        }
        state => state == 2,
    }
}

pub fn enable() -> Result<()> {
    let conn = store::open()?;
    store::set_state(&conn, ENABLED_KEY, "1")?;
    ENABLED.store(2, Ordering::Relaxed);
    Ok(())
}

/// Remove the stored message text and turn the feature off.
pub fn delete() -> Result<()> {
    if store::path().exists() {
        let conn = store::open()?;
        store::clear_messages(&conn)?;
        store::set_state(&conn, ENABLED_KEY, "0")?;
        store::set_state(&conn, PROGRESS_KEY, "")?;
    }
    match std::fs::remove_dir_all(old_dir()) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(e).context("couldn't remove the old search index"),
    }
    ENABLED.store(1, Ordering::Relaxed);
    Ok(())
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
    /// True when the download stopped, or older text was removed, because
    /// of a limit in the settings rather than reaching the start of
    /// history. If the limit is later loosened, the download carries on.
    #[serde(default)]
    pub limited: bool,
}

impl Progress {
    pub fn load() -> Self {
        let from_store = store::path()
            .exists()
            .then(|| store::open().ok())
            .flatten()
            .and_then(|conn| store::state(&conn, PROGRESS_KEY))
            .and_then(|json| serde_json::from_str(&json).ok());
        from_store
            .or_else(|| {
                // Not brought across yet: the old folder's own record.
                std::fs::read(old_dir().join("state.json"))
                    .ok()
                    .and_then(|bytes| serde_json::from_slice(&bytes).ok())
            })
            .unwrap_or_default()
    }

    fn save(&self, conn: &rusqlite::Connection) {
        if let Ok(json) = serde_json::to_string(self) {
            let _ = store::set_state(conn, PROGRESS_KEY, &json);
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

/// Bring an index kept as text files into the database. Returns what was
/// found, or `None` if there was nothing to bring. The old files are
/// removed afterwards, unless `keep` or any message failed to arrive.
pub fn upgrade(keep: bool) -> Result<Option<store::Imported>> {
    let old = old_dir();
    if !old.join("messages.tsv").exists() {
        return Ok(None);
    }
    let mut conn = store::open()?;
    let imported = store::import_text_index(&mut conn, &old)?;
    // Carry over how far the download had got, and that it was wanted.
    let progress = Progress::load();
    Progress {
        stored: store::message_count(&conn),
        ..progress
    }
    .save(&conn);
    store::set_state(&conn, ENABLED_KEY, "1")?;
    ENABLED.store(2, Ordering::Relaxed);
    if imported.missing > 0 {
        crate::config::log(&format!(
            "search index upgrade: {} messages did not arrive; the old index is kept",
            imported.missing
        ));
    } else if !keep {
        let _ = std::fs::remove_dir_all(&old);
    }
    Ok(Some(imported))
}

const MB: u64 = 1024 * 1024;

/// The settings' limits: the earliest date to keep (if years are limited)
/// and the most bytes to take (if size is).
fn limits() -> (Option<i64>, Option<u64>) {
    let search = &crate::config::settings().search;
    let years = i64::from(search.years);
    let cutoff = (years > 0)
        .then(|| chrono::Utc::now().timestamp_millis() - years * 365 * 24 * 3600 * 1000);
    let cap = (search.max_mb > 0).then(|| search.max_mb.saturating_mul(MB));
    (cutoff, cap)
}

/// Bring the stored text within the limits: drop what is older than the
/// years allowed, then the oldest of the rest if it is still too big.
/// Returns how many messages were removed.
pub fn trim(conn: &rusqlite::Connection, progress: &mut Progress) -> u64 {
    let (cutoff, cap) = limits();
    let before = store::message_count(conn);
    let mut removed = 0;
    if let Some(cutoff) = cutoff {
        removed += store::delete_before(conn, cutoff);
    }
    // Deleting leaves the file its old size until it is compacted, which
    // rewrites it, so that is only done after a real clear-out.
    if removed * 20 >= before.max(1) {
        store::compact(conn);
    }
    if let Some(cap) = cap {
        // How much a message takes varies with its age, so removing a
        // share of the messages is only an estimate of the space freed:
        // measure, remove, compact and measure again until it fits.
        for _ in 0..5 {
            let size = store::size_on_disk();
            let held = store::message_count(conn);
            if size <= cap || held == 0 {
                break;
            }
            // Aim a little under, so new messages do not tip it back over.
            let target = cap / 10 * 9;
            let excess = (size - target) as f64 / size as f64;
            let gone = store::delete_oldest(conn, (held as f64 * excess).ceil() as u64);
            if gone == 0 {
                break;
            }
            removed += gone;
            store::compact(conn);
        }
    }
    if removed > 0 {
        progress.limited = true;
        progress.complete = true;
        progress.stored = store::message_count(conn);
        if let Some(oldest) = store::oldest_date(conn) {
            progress.oldest = oldest;
        }
        progress.save(conn);
    }
    removed
}

fn store_page(conn: &mut rusqlite::Connection, messages: &[Message]) -> Result<u64> {
    let owned = store::rows_from(messages);
    let rows: Vec<store::Row> = owned
        .iter()
        .map(|(guid, chat, date, from_me, sender, text)| store::Row {
            guid,
            chat,
            date: *date,
            from_me: *from_me,
            sender,
            text,
        })
        .collect();
    store::put(conn, &rows)
}

/// Keep the store complete: catch up on anything newer than what is held,
/// follow history back to the beginning, and add messages as they arrive.
pub async fn run(
    api: Api,
    tx: UnboundedSender<AppEvent>,
    mut live: UnboundedReceiver<Message>,
    max_pages: Option<usize>,
) -> Result<()> {
    // An index from before the database: bring it across first. Half a
    // million messages take a few seconds, so do it off the async threads.
    if old_dir().join("messages.tsv").exists() {
        let _ = tx.send(AppEvent::Status("Upgrading the search index…".into()));
        match tokio::task::spawn_blocking(|| upgrade(false)).await? {
            Ok(_) => {
                let _ = tx.send(AppEvent::Status("Search index upgraded".into()));
            }
            Err(e) => crate::config::log(&format!("search index upgrade failed: {e:#}")),
        }
    }
    let mut conn = store::open()?;
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
            progress.stored += store_page(&mut conn, &page)?;
            progress.newest = progress.newest.max(latest);
        }
        progress.save(&conn);
        report(&progress);
        if got < PAGE {
            break;
        }
    }

    // Apply the limits to what is already held.
    trim(&conn, &mut progress);
    let (cutoff, cap) = limits();
    // If the download was held back by a limit that has since been
    // loosened, there is older history to fetch again.
    if progress.complete && progress.limited {
        // A month of slack: text trimmed to the limit sits right at it,
        // and must not be mistaken for a limit that has moved.
        let month = 30 * 24 * 3600 * 1000;
        let room_in_time = cutoff.is_none_or(|cutoff| progress.oldest > cutoff + month);
        let room_on_disk = cap.is_none_or(|cap| store::size_on_disk() < cap / 10 * 8);
        if room_in_time && room_on_disk {
            progress.complete = false;
            progress.limited = false;
        }
    }
    report(&progress);

    // Then history, newest first, so recent messages are searchable early,
    // stopping at either limit.
    while !progress.complete {
        let far_enough = cutoff.is_some_and(|cutoff| progress.oldest > 0 && progress.oldest < cutoff);
        let full = cap.is_some_and(|cap| pages % 10 == 0 && store::size_on_disk() >= cap);
        if far_enough || full {
            progress.complete = true;
            progress.limited = true;
            trim(&conn, &mut progress);
            progress.save(&conn);
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
            progress.stored += store_page(&mut conn, &page)?;
            let oldest = page.iter().map(|m| m.date_created).min().unwrap_or(0);
            let newest = page.iter().map(|m| m.date_created).max().unwrap_or(0);
            progress.oldest = if progress.oldest == 0 { oldest } else { progress.oldest.min(oldest) };
            progress.newest = progress.newest.max(newest);
            if page.len() < PAGE {
                progress.complete = true;
            }
        }
        progress.save(&conn);
        report(&progress);
        // Leave the server room to serve the app itself.
        tokio::time::sleep(PAUSE).await;
    }
    if max_pages.is_some() {
        return Ok(());
    }

    // From here on, keep in step with what the app hears: new messages are
    // added, an edited one has its text replaced, an unsent one is removed.
    while let Some(message) = live.recv().await {
        if message.date_retracted.is_some() {
            store::remove(&conn, &message.guid);
            continue;
        }
        progress.stored += store_page(&mut conn, std::slice::from_ref(&message))?;
        if message.date_created > progress.newest {
            progress.newest = message.date_created;
        }
        progress.save(&conn);
    }
    Ok(())
}

/// Find messages containing every word of `query`, newest first. Words
/// match from their start and case and accents are ignored. Blocking:
/// call it off the UI thread.
pub fn search(query: &str, limit: usize) -> Result<Vec<Hit>> {
    if !store::path().exists() {
        return Ok(Vec::new());
    }
    store::search(&store::open()?, query, limit)
}

/// The conversations those messages are in, the one with the most first.
/// Blocking, like [`search`].
pub fn mentions(query: &str) -> Result<Vec<Mention>> {
    if !store::path().exists() {
        return Ok(Vec::new());
    }
    store::mentions(&store::open()?, query)
}

#[cfg(test)]
mod tests {
    use super::*;

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
