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

pub use crate::store::Hit;

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

    // Then history, newest first, so recent messages are searchable early.
    // With a limit in the settings, stop once that far back.
    let years = i64::from(crate::config::settings().search.years);
    let cutoff = (years > 0)
        .then(|| chrono::Utc::now().timestamp_millis() - years * 365 * 24 * 3600 * 1000);
    while !progress.complete {
        if cutoff.is_some_and(|cutoff| progress.oldest > 0 && progress.oldest < cutoff) {
            progress.complete = true;
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
