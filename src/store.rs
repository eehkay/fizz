//! Fizz's local database: one SQLite file holding what the app keeps
//! between runs. It replaces the separate files that grew up one feature
//! at a time (a text index for search, a JSON file of read markers).
//!
//! The file is ordinary, unencrypted SQLite, readable only by its owner,
//! which is how Messages keeps its own database on a Mac. Pictures and
//! other media are not in here; they stay as files in the cache.

use anyhow::{Context, Result};
use rusqlite::{Connection, OptionalExtension, params};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

use crate::model::Message;

/// Bumped whenever the tables change; `migrate` brings older files up.
const SCHEMA_VERSION: i64 = 3;

pub fn folder() -> PathBuf {
    dirs::data_local_dir()
        .unwrap_or_else(std::env::temp_dir)
        .join("fizz")
}

pub fn path() -> PathBuf {
    folder().join("fizz.db")
}

/// Open the database, creating or upgrading it as needed.
pub fn open() -> Result<Connection> {
    let folder = folder();
    std::fs::create_dir_all(&folder).context("couldn't create Fizz's data folder")?;
    let path = path();
    let fresh = !path.exists();
    let conn = Connection::open(&path).context("couldn't open Fizz's database")?;
    #[cfg(unix)]
    if fresh {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&folder, std::fs::Permissions::from_mode(0o700));
        let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
    }
    prepare(&conn)?;
    Ok(conn)
}

/// Settings every connection needs, then the tables.
fn prepare(conn: &Connection) -> Result<()> {
    // Readers and the one writer do not block each other in this mode,
    // which matters because search runs while history is still arriving.
    conn.pragma_update(None, "journal_mode", "WAL")?;
    conn.pragma_update(None, "synchronous", "NORMAL")?;
    conn.busy_timeout(std::time::Duration::from_secs(5))?;
    migrate(conn)
}

fn migrate(conn: &Connection) -> Result<()> {
    let version: i64 = conn.query_row("PRAGMA user_version", [], |row| row.get(0))?;
    if version >= SCHEMA_VERSION {
        return Ok(());
    }
    if version < 1 {
        conn.execute_batch(
            r#"
            BEGIN;
            -- Chats are numbered so each message carries a small number
            -- rather than repeating a long id.
            CREATE TABLE chats (
                id   INTEGER PRIMARY KEY,
                guid TEXT NOT NULL UNIQUE
            );
            -- A message's id is a 36-character code; it is kept in its
            -- 16-byte form (see `pack`), which more than halves both this
            -- column and the index that finds a message by it.
            CREATE TABLE messages (
                id      INTEGER PRIMARY KEY,
                guid    NOT NULL UNIQUE,
                chat    INTEGER NOT NULL REFERENCES chats(id),
                date    INTEGER NOT NULL,
                from_me INTEGER NOT NULL,
                sender  TEXT NOT NULL DEFAULT '',
                text    TEXT NOT NULL DEFAULT ''
            );

            -- Full-text search over message text. It indexes the messages
            -- table rather than keeping a second copy of the text, and
            -- stores no word positions, which keeps it small; the triggers
            -- below keep it in step with every change.
            CREATE VIRTUAL TABLE messages_fts USING fts5(
                text,
                content = 'messages',
                content_rowid = 'id',
                tokenize = 'unicode61 remove_diacritics 2',
                detail = 'none',
                columnsize = 0
            );
            CREATE TRIGGER messages_added AFTER INSERT ON messages BEGIN
                INSERT INTO messages_fts(rowid, text) VALUES (new.id, new.text);
            END;
            CREATE TRIGGER messages_removed AFTER DELETE ON messages BEGIN
                INSERT INTO messages_fts(messages_fts, rowid, text)
                VALUES ('delete', old.id, old.text);
            END;
            CREATE TRIGGER messages_edited AFTER UPDATE OF text ON messages BEGIN
                INSERT INTO messages_fts(messages_fts, rowid, text)
                VALUES ('delete', old.id, old.text);
                INSERT INTO messages_fts(rowid, text) VALUES (new.id, new.text);
            END;

            -- When each chat was last looked at.
            CREATE TABLE seen (
                chat TEXT PRIMARY KEY,
                at   INTEGER NOT NULL
            );
            -- Odds and ends that are one value each.
            CREATE TABLE state (
                key   TEXT PRIMARY KEY,
                value TEXT NOT NULL
            );
            PRAGMA user_version = 1;
            COMMIT;
            "#,
        )
        .context("couldn't set up Fizz's database")?;
    }
    if version < 2 {
        conn.execute_batch(
            r#"
            BEGIN;
            -- A message kept whole, as the app shows it (see `encode`).
            -- Empty for one known only by its text, for search.
            ALTER TABLE messages ADD COLUMN body BLOB;
            -- The stretch of each conversation that is held whole, with
            -- nothing missing inside it (see "what is held whole" below).
            ALTER TABLE chats ADD COLUMN have_from INTEGER;
            ALTER TABLE chats ADD COLUMN have_to INTEGER;
            -- Reading a conversation's page is by chat and date; only the
            -- messages kept whole are in it, so it stays small.
            CREATE INDEX messages_kept ON messages(chat, date) WHERE body IS NOT NULL;
            PRAGMA user_version = 2;
            COMMIT;
            "#,
        )
        .context("couldn't upgrade Fizz's database")?;
    }
    if version < 3 {
        conn.execute_batch(
            r#"
            BEGIN;
            -- Which folder of the chat list a conversation has been put
            -- in. A folder is nothing but its name here: it exists for as
            -- long as something is in it.
            CREATE TABLE filed (
                chat   TEXT PRIMARY KEY,
                folder TEXT NOT NULL
            );
            PRAGMA user_version = 3;
            COMMIT;
            "#,
        )
        .context("couldn't upgrade Fizz's database")?;
    }
    Ok(())
}

// ----- one-value settings ----------------------------------------------

pub fn state(conn: &Connection, key: &str) -> Option<String> {
    conn.query_row("SELECT value FROM state WHERE key = ?1", [key], |row| row.get(0))
        .optional()
        .ok()
        .flatten()
}

pub fn set_state(conn: &Connection, key: &str, value: &str) -> Result<()> {
    conn.execute(
        "INSERT INTO state(key, value) VALUES (?1, ?2)
         ON CONFLICT(key) DO UPDATE SET value = excluded.value",
        params![key, value],
    )?;
    Ok(())
}

// ----- read markers ----------------------------------------------------

pub fn seen(conn: &Connection) -> HashMap<String, i64> {
    let Ok(mut statement) = conn.prepare("SELECT chat, at FROM seen") else {
        return HashMap::new();
    };
    statement
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
        .map(|rows| rows.flatten().collect())
        .unwrap_or_default()
}

pub fn set_seen(conn: &Connection, chat: &str, at: i64) -> Result<()> {
    conn.execute(
        "INSERT INTO seen(chat, at) VALUES (?1, ?2)
         ON CONFLICT(chat) DO UPDATE SET at = excluded.at",
        params![chat, at],
    )?;
    Ok(())
}

pub fn forget_seen(conn: &Connection, chat: &str) {
    let _ = conn.execute("DELETE FROM seen WHERE chat = ?1", [chat]);
}

// ----- folders ---------------------------------------------------------

/// Every conversation that is in a folder, and which.
pub fn filed(conn: &Connection) -> HashMap<String, String> {
    let Ok(mut statement) = conn.prepare("SELECT chat, folder FROM filed") else {
        return HashMap::new();
    };
    statement
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
        .map(|rows| rows.flatten().collect())
        .unwrap_or_default()
}

/// Put a conversation in a folder, or with `None` take it out of one.
pub fn file(conn: &Connection, chat: &str, folder: Option<&str>) -> Result<()> {
    match folder {
        Some(folder) => conn.execute(
            "INSERT INTO filed(chat, folder) VALUES (?1, ?2)
             ON CONFLICT(chat) DO UPDATE SET folder = excluded.folder",
            params![chat, folder],
        )?,
        None => conn.execute("DELETE FROM filed WHERE chat = ?1", [chat])?,
    };
    Ok(())
}

// ----- messages --------------------------------------------------------

fn chat_number(conn: &Connection, guid: &str) -> Result<i64> {
    conn.execute("INSERT OR IGNORE INTO chats(guid) VALUES (?1)", [guid])?;
    Ok(conn.query_row("SELECT id FROM chats WHERE guid = ?1", [guid], |row| row.get(0))?)
}

/// A message id in the form it is stored: the 16 bytes behind the usual
/// `XXXXXXXX-XXXX-XXXX-XXXX-XXXXXXXXXXXX`, or the text itself for an id in
/// any other shape, so nothing is ever lost by packing.
fn pack(guid: &str) -> rusqlite::types::Value {
    use rusqlite::types::Value;
    let bytes = guid.as_bytes();
    let dashes = [8, 13, 18, 23];
    let standard = bytes.len() == 36
        && bytes.iter().enumerate().all(|(i, b)| {
            if dashes.contains(&i) {
                *b == b'-'
            } else {
                b.is_ascii_digit() || (b'A'..=b'F').contains(b)
            }
        });
    if !standard {
        return Value::Text(guid.to_string());
    }
    let hex: Vec<u8> = bytes.iter().copied().filter(|b| *b != b'-').collect();
    let nibble = |b: u8| if b.is_ascii_digit() { b - b'0' } else { b - b'A' + 10 };
    Value::Blob(hex.chunks(2).map(|pair| nibble(pair[0]) << 4 | nibble(pair[1])).collect())
}

/// The reverse of [`pack`].
fn unpack(stored: rusqlite::types::ValueRef) -> String {
    use rusqlite::types::ValueRef;
    match stored {
        ValueRef::Blob(bytes) if bytes.len() == 16 => {
            let hex: String = bytes.iter().map(|b| format!("{b:02X}")).collect();
            format!(
                "{}-{}-{}-{}-{}",
                &hex[0..8],
                &hex[8..12],
                &hex[12..16],
                &hex[16..20],
                &hex[20..32]
            )
        }
        ValueRef::Text(text) => String::from_utf8_lossy(text).into_owned(),
        ValueRef::Blob(bytes) => String::from_utf8_lossy(bytes).into_owned(),
        _ => String::new(),
    }
}

/// One message as the store keeps it.
pub struct Row<'a> {
    pub guid: &'a str,
    pub chat: &'a str,
    pub date: i64,
    pub from_me: bool,
    pub sender: &'a str,
    pub text: &'a str,
}

/// Store messages, replacing the text of any already held (which is how an
/// edit reaches search). Returns how many were new. One transaction, so a
/// page of history is either all there or not at all.
pub fn put(conn: &mut Connection, rows: &[Row]) -> Result<u64> {
    let tx = conn.transaction()?;
    let mut added = 0;
    {
        let mut numbers: HashMap<&str, i64> = HashMap::new();
        let mut insert = tx.prepare_cached(
            "INSERT INTO messages(guid, chat, date, from_me, sender, text)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)
             ON CONFLICT(guid) DO NOTHING",
        )?;
        let mut update = tx.prepare_cached(
            "UPDATE messages SET text = ?2 WHERE guid = ?1 AND text <> ?2",
        )?;
        for row in rows {
            let chat = match numbers.get(row.chat) {
                Some(n) => *n,
                None => {
                    let n = chat_number(&tx, row.chat)?;
                    numbers.insert(row.chat, n);
                    n
                }
            };
            let guid = pack(row.guid);
            let inserted = insert.execute(params![
                guid,
                chat,
                row.date,
                row.from_me,
                row.sender,
                row.text
            ])?;
            if inserted > 0 {
                added += 1;
            } else {
                // Already held: new text means it was edited.
                update.execute(params![guid, row.text])?;
            }
        }
    }
    tx.commit()?;
    Ok(added)
}

/// The rows worth keeping from a batch of the server's messages: things
/// people said, not tapbacks, events or pictures with no text.
pub fn rows_from(messages: &[Message]) -> Vec<(String, String, i64, bool, String, String)> {
    messages
        .iter()
        .filter(|m| m.reaction().is_none() && m.item_type == 0)
        .filter_map(|m| {
            let text = m.body();
            let chat = m.chats.first()?;
            if text.is_empty() || chat.guid.is_empty() || m.guid.is_empty() {
                return None;
            }
            let sender = match (&m.handle, m.is_from_me) {
                (Some(h), false) => h.address.clone(),
                _ => String::new(),
            };
            Some((
                m.guid.clone(),
                chat.guid.clone(),
                m.date_created,
                m.is_from_me,
                sender,
                text,
            ))
        })
        .collect()
}

/// Take a message out of the store (it was unsent or deleted).
pub fn remove(conn: &Connection, guid: &str) {
    let _ = conn.execute("DELETE FROM messages WHERE guid = ?1", [pack(guid)]);
}

pub fn message_count(conn: &Connection) -> u64 {
    conn.query_row("SELECT count(*) FROM messages", [], |row| row.get::<_, i64>(0))
        .unwrap_or(0) as u64
}

/// Remove every stored message and hand the space back to the system.
pub fn clear_messages(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        "DELETE FROM messages;
         DELETE FROM chats;
         DELETE FROM state WHERE key IN ('sync_base', 'synced_to');
         INSERT INTO messages_fts(messages_fts) VALUES ('rebuild');
         VACUUM;",
    )?;
    Ok(())
}

/// Remove messages older than `date` (milliseconds). Returns how many.
pub fn delete_before(conn: &Connection, date: i64) -> u64 {
    let gone = conn
        .execute("DELETE FROM messages WHERE date < ?1", [date])
        .unwrap_or(0) as u64;
    if gone > 0 {
        forget_before(conn, date);
    }
    gone
}

/// Remove the `count` oldest messages. Returns how many went.
pub fn delete_oldest(conn: &Connection, count: u64) -> u64 {
    let gone = conn
        .execute(
            "DELETE FROM messages WHERE id IN
             (SELECT id FROM messages ORDER BY date ASC LIMIT ?1)",
            [count as i64],
        )
        .unwrap_or(0) as u64;
    if gone > 0
        && let Some(oldest) = oldest_date(conn)
    {
        forget_before(conn, oldest);
    }
    gone
}

/// The date of the oldest message held.
pub fn oldest_date(conn: &Connection) -> Option<i64> {
    conn.query_row("SELECT min(date) FROM messages", [], |row| row.get(0))
        .ok()
        .flatten()
}

/// Hand space freed by deletions back to the system. Rewrites the file,
/// so it is for after a real clear-out, not every small change.
pub fn compact(conn: &Connection) {
    // The search index keeps removed entries around until it is merged
    // down; without this the file barely shrinks after a big deletion.
    let _ = conn.execute_batch(
        "INSERT INTO messages_fts(messages_fts) VALUES ('optimize');
         VACUUM;
         PRAGMA wal_checkpoint(TRUNCATE);",
    );
}

// ----- whole messages --------------------------------------------------
//
// Besides text for search, the store keeps messages whole, so a
// conversation can be shown without asking the server. A message is kept
// as JSON in `body`, less what its row already says (see `encode`).
//
// What is held whole is recorded as stretches of time with nothing missing
// inside them, because a page with a hole in it reads as a conversation
// that never had those messages:
//
//  - each chat has one stretch, `have_from..have_to`, grown by the pages
//    fetched for it;
//  - one stretch covers every chat at once, `sync_base..synced_to`: the
//    time over which Fizz has taken in everything that arrived.
//
// A chat's page may be read from here only inside those (see `covered`).

/// `have_from` for a conversation held from its very first message.
pub const BEGINNING: i64 = 0;
/// A page fetched as the newest may be missing what arrived while it was
/// on its way, so nothing this recent is taken to have been deleted.
const SETTLING: i64 = 60_000;

/// A message as kept in `body`: itself, without what the row's other
/// columns hold (its id, date, direction and, usually, text), without the
/// chat it is in, and with Apple's link-preview archive cut down to the
/// one thing read from it, the title.
fn encode(message: &Message, text: &str) -> Vec<u8> {
    let mut message = message.clone();
    if message.text.is_none() {
        // The words are in the attributed form only: keep them as text.
        let words = message
            .attributed_body
            .as_ref()
            .and_then(|b| b.get(0))
            .and_then(|b| b.get("string"))
            .and_then(|s| s.as_str())
            .unwrap_or_default();
        if !words.is_empty() {
            message.text = Some(words.to_string());
        }
    }
    message.attributed_body = None;
    message.payload_data = message.link_title().map(|title| {
        serde_json::json!([{ "$objects": [{ "title": { "UID": 1 }, "originalURL": { "UID": 1 } }, title] }])
    });
    message.chats.clear();
    let mut value = serde_json::to_value(&message).unwrap_or_default();
    if let Some(fields) = value.as_object_mut() {
        for key in ["guid", "dateCreated", "isFromMe"] {
            fields.remove(key);
        }
        if !text.is_empty() && fields.get("text").and_then(|t| t.as_str()) == Some(text) {
            fields.remove("text");
        }
        // Everything left out here comes back as the same value when read.
        fields.retain(|key, v| {
            !(v.is_null()
                || *v == serde_json::Value::Bool(false)
                || v.as_array().is_some_and(|a| a.is_empty())
                || (matches!(key.as_str(), "error" | "itemType") && v.as_i64() == Some(0)))
        });
    }
    serde_json::to_vec(&value).unwrap_or_default()
}

/// The reverse of [`encode`], given the row's other columns.
fn decode(guid: String, date: i64, from_me: bool, text: String, body: &[u8]) -> Option<Message> {
    let mut message: Message = serde_json::from_slice(body).ok()?;
    message.guid = guid;
    message.date_created = date;
    message.is_from_me = from_me;
    if message.text.is_none() && !text.is_empty() {
        message.text = Some(text);
    }
    Some(message)
}

/// The text a message is found by: what was said, and nothing for a
/// tapback, an event such as someone leaving, or an unsent message.
fn searchable(message: &Message) -> String {
    if message.reaction().is_some() || message.item_type != 0 || message.date_retracted.is_some() {
        return String::new();
    }
    message.body()
}

fn sender_of(message: &Message) -> &str {
    match (&message.handle, message.is_from_me) {
        (Some(handle), false) => &handle.address,
        _ => "",
    }
}

/// What storing a batch of whole messages did.
#[derive(Debug, Default, PartialEq)]
struct Kept {
    /// New messages with something to search for.
    added: u64,
    /// True if any was already held under another conversation's id.
    elsewhere: bool,
}

/// Store messages whole. Each goes under `chat`, or with none given under
/// the conversation it names itself (a page fetched for one conversation
/// does not repeat which on every message).
fn keep_in(tx: &rusqlite::Transaction, chat: Option<&str>, messages: &[Message]) -> Result<Kept> {
    let mut kept = Kept::default();
    let mut numbers: HashMap<String, i64> = HashMap::new();
    let mut find = tx.prepare_cached("SELECT chat, text, body FROM messages WHERE guid = ?1")?;
    let mut insert = tx.prepare_cached(
        "INSERT INTO messages(guid, chat, date, from_me, sender, text, body)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
    )?;
    let mut update = tx.prepare_cached("UPDATE messages SET date = ?2, body = ?3 WHERE guid = ?1")?;
    let mut retext = tx.prepare_cached("UPDATE messages SET text = ?2 WHERE guid = ?1")?;
    for message in messages {
        let Some(chat) = chat.or_else(|| message.chats.first().map(|c| c.guid.as_str())) else {
            continue;
        };
        if message.guid.is_empty() || chat.is_empty() || message.pending || message.failed.is_some() {
            continue;
        }
        let number = match numbers.get(chat) {
            Some(n) => *n,
            None => {
                let n = chat_number(tx, chat)?;
                numbers.insert(chat.to_string(), n);
                n
            }
        };
        let guid = pack(&message.guid);
        let text = searchable(message);
        let held: Option<(i64, String, Option<Vec<u8>>)> = find
            .query_row([&guid], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))
            .optional()?;
        match held {
            None => {
                insert.execute(params![
                    guid,
                    number,
                    message.date_created,
                    message.is_from_me,
                    sender_of(message),
                    text,
                    encode(message, &text),
                ])?;
                if !text.is_empty() {
                    kept.added += 1;
                }
            }
            Some((held_chat, held_text, held_body)) => {
                kept.elsewhere |= held_chat != number;
                // A link's title comes only with a conversation's own
                // pages; hearing of the message again another way must
                // not lose it.
                let mut message = std::borrow::Cow::Borrowed(message);
                if message.payload_data.is_none()
                    && let Some(old) = held_body
                        .as_deref()
                        .and_then(|body| serde_json::from_slice::<Message>(body).ok())
                    && old.payload_data.is_some()
                {
                    message.to_mut().payload_data = old.payload_data;
                }
                update.execute(params![guid, message.date_created, encode(&message, &text)])?;
                if held_text != text {
                    retext.execute(params![guid, text])?;
                }
            }
        }
    }
    Ok(kept)
}

/// Store whole messages that each say which conversation they are in:
/// what arrives live, and what is caught up on. Returns how many were new
/// and have text.
pub fn keep(conn: &mut Connection, messages: &[Message]) -> Result<u64> {
    let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    let kept = keep_in(&tx, None, messages)?;
    tx.commit()?;
    Ok(kept.added)
}

fn chat_stretch(conn: &Connection, chat: &str) -> Option<(i64, i64)> {
    conn.query_row(
        "SELECT have_from, have_to FROM chats WHERE guid = ?1",
        [chat],
        |row| Ok((row.get::<_, Option<i64>>(0)?, row.get::<_, Option<i64>>(1)?)),
    )
    .optional()
    .ok()
    .flatten()
    .and_then(|(from, to)| Some((from?, to?)))
}

/// The time over which everything that arrived, in any conversation, was
/// taken in: from when that began to how far it has got.
pub fn synced(conn: &Connection) -> Option<(i64, i64)> {
    let number = |key: &str| state(conn, key)?.parse::<i64>().ok();
    Some((number("sync_base")?, number("synced_to")?))
}

/// Begin a new such stretch at `at`: whatever was taken in before is no
/// longer joined to the present.
pub fn start_sync(conn: &Connection, at: i64) -> Result<()> {
    set_state(conn, "sync_base", &at.to_string())?;
    set_state(conn, "synced_to", &at.to_string())
}

/// Everything up to `to` has now been taken in. Never moves backwards,
/// whichever Fizz says so.
pub fn set_synced_to(conn: &Connection, to: i64) -> Result<()> {
    conn.execute(
        "UPDATE state SET value = ?1 WHERE key = 'synced_to' AND CAST(value AS INTEGER) < ?1",
        [to],
    )?;
    Ok(())
}

/// The stretch of a conversation that is held whole and reaches furthest
/// towards the present: its own, the one covering every chat, or the two
/// together where they meet.
pub fn covered(conn: &Connection, chat: &str) -> Option<(i64, i64)> {
    match (chat_stretch(conn, chat), synced(conn)) {
        (Some(own), Some(all)) => {
            if own.0 <= all.1 && all.0 <= own.1 {
                Some((own.0.min(all.0), own.1.max(all.1)))
            } else if own.1 >= all.1 {
                Some(own)
            } else {
                Some(all)
            }
        }
        (own, all) => own.or(all),
    }
}

/// Messages older than `date` have been removed: nothing before it is
/// held whole any more.
fn forget_before(conn: &Connection, date: i64) {
    let _ = conn.execute(
        "UPDATE chats SET have_from = ?1 WHERE have_from IS NOT NULL AND have_from < ?1",
        [date],
    );
    let _ = conn.execute(
        "UPDATE chats SET have_from = NULL, have_to = NULL WHERE have_from > have_to",
        [],
    );
    if let Some((base, to)) = synced(conn)
        && base < date
    {
        let _ = set_state(conn, "sync_base", &date.min(to).to_string());
    }
}

/// Which part of a conversation a page from the server is.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Shape {
    /// The newest `limit` messages.
    Latest { limit: usize },
    /// The `limit` before a date, going back from what is on screen.
    Older { before: i64, limit: usize },
    /// Anything else (around an old message, or forward from one): kept,
    /// but it says nothing about what lies either side of it.
    Loose,
}

/// What storing a page found out.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct Stored {
    /// Whether the store can speak for this stretch of the conversation.
    /// When false the page was kept and nothing more is claimed.
    pub vouched: bool,
    /// For the newest page: whether it reaches back as far as what was
    /// already held, so the two run on without a hole between them.
    pub joined: bool,
    /// Messages that were held in the page's stretch but are not in the
    /// page: they have been deleted since, and are deleted here too.
    pub gone: Vec<String>,
    /// New messages with text.
    pub added: u64,
}

/// Store a page of one conversation exactly as the server gave it: add
/// and update what is in it, remove what it shows to have gone, and note
/// how much of the conversation is now held whole. `now` is when the page
/// was asked for.
pub fn keep_page(conn: &mut Connection, chat: &str, page: &[Message], shape: Shape, now: i64) -> Result<Stored> {
    let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    let before = covered(&tx, chat);
    let kept = keep_in(&tx, Some(chat), page)?;
    let mut stored = Stored {
        added: kept.added,
        ..Stored::default()
    };
    let limit = match shape {
        Shape::Latest { limit } | Shape::Older { limit, .. } => limit,
        Shape::Loose => 0,
    };
    // Held under another id as well: this conversation goes by two names
    // and what is stored cannot be trusted to be all of it.
    if shape == Shape::Loose || kept.elsewhere {
        tx.commit()?;
        return Ok(stored);
    }
    stored.vouched = true;
    let number = chat_number(&tx, chat)?;
    let whole = page.len() < limit;
    let oldest = page.iter().map(|m| m.date_created).min();
    // The Mac's clock dates the messages, and may run ahead of this one.
    let now = now.max(page.iter().map(|m| m.date_created).max().unwrap_or(now));
    // The stretch the page speaks for: everything of this conversation
    // strictly between these two dates is in it.
    let low = if whole { BEGINNING - 1 } else { oldest.unwrap_or(now) };
    let (high, reach) = match shape {
        Shape::Latest { .. } => (now - SETTLING, now),
        Shape::Older { before, .. } => (before, before),
        Shape::Loose => unreachable!(),
    };

    // What it shows to have gone.
    let in_page: std::collections::HashSet<&str> = page.iter().map(|m| m.guid.as_str()).collect();
    let missing: Vec<(i64, String)> = {
        let mut statement = tx.prepare_cached(
            "SELECT id, guid FROM messages
             WHERE chat = ?1 AND body IS NOT NULL AND date > ?2 AND date < ?3",
        )?;
        statement
            .query_map(params![number, low, high], |row| Ok((row.get(0)?, unpack(row.get_ref(1)?))))?
            .collect::<rusqlite::Result<Vec<_>>>()?
            .into_iter()
            .filter(|(_, guid)| !in_page.contains(guid.as_str()))
            .collect()
    };
    for (id, guid) in missing {
        tx.execute("DELETE FROM messages WHERE id = ?1", [id])?;
        stored.gone.push(guid);
    }

    // How much is now held whole.
    let from = if whole { BEGINNING } else { low };
    let stretch = match (shape, before) {
        (Shape::Latest { .. }, Some((held_from, held_to))) if whole || from <= held_to => {
            stored.joined = true;
            Some((held_from.min(from), reach))
        }
        (Shape::Latest { .. }, _) => Some((from, reach)),
        // Going back: only a page that starts inside what is held adds to it.
        (Shape::Older { before, .. }, Some((held_from, held_to)))
            if held_from <= before && before <= held_to.saturating_add(1) =>
        {
            Some((held_from.min(from), held_to))
        }
        _ => None,
    };
    if let Some((from, to)) = stretch {
        tx.execute(
            "UPDATE chats SET have_from = ?2, have_to = ?3 WHERE id = ?1",
            params![number, from, to],
        )?;
    }
    tx.commit()?;
    Ok(stored)
}

/// A page read from the store instead of the server.
#[derive(Debug, Default)]
pub struct Local {
    /// Newest first, as the server gives a page.
    pub messages: Vec<Message>,
    /// Whether this is all the page there is to have: as many as were
    /// asked for, or everything back to the conversation's first message.
    pub complete: bool,
    /// Whether it reaches the conversation's first message.
    pub reaches_start: bool,
}

/// Up to `limit` of a conversation's messages older than `before` (or its
/// newest), from what is held whole. Empty when the store cannot vouch
/// for that part of the conversation.
pub fn page(conn: &Connection, chat: &str, before: Option<i64>, limit: usize) -> Local {
    let Some((from, to)) = covered(conn, chat) else {
        return Local::default();
    };
    // Going back from a message newer than anything held: what lies
    // between it and the store is unknown.
    if before.is_some_and(|before| before - 1 > to) {
        return Local::default();
    }
    let read = || -> rusqlite::Result<Vec<Message>> {
        let mut statement = conn.prepare_cached(
            "SELECT m.guid, m.date, m.from_me, m.text, m.body
             FROM messages m JOIN chats c ON c.id = m.chat
             WHERE c.guid = ?1 AND m.body IS NOT NULL
               AND m.date >= ?2 AND m.date <= ?3 AND m.date < ?4
             ORDER BY m.date DESC LIMIT ?5",
        )?;
        // Nothing dated after the stretch either: something that arrived
        // while a catch-up was cut short lies beyond a hole.
        let rows = statement.query_map(
            params![chat, from, to, before.unwrap_or(i64::MAX), limit as i64],
            |row| {
                Ok(decode(
                    unpack(row.get_ref(0)?),
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get_ref(4)?.as_blob()?,
                ))
            },
        )?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?.into_iter().flatten().collect())
    };
    let messages = read().unwrap_or_default();
    let full = messages.len() >= limit;
    Local {
        complete: full || from == BEGINNING,
        reaches_start: !full && from == BEGINNING,
        messages,
    }
}

/// A conversation was deleted: remove its messages, text and all.
pub fn forget_chat(conn: &Connection, chat: &str) {
    let _ = conn.execute(
        "DELETE FROM messages WHERE chat = (SELECT id FROM chats WHERE guid = ?1)",
        [chat],
    );
    let _ = conn.execute(
        "UPDATE chats SET have_from = NULL, have_to = NULL WHERE guid = ?1",
        [chat],
    );
}

/// Whole messages are no longer wanted kept. What word search needs of
/// them (their text) stays if `keep_text`; the rest goes.
pub fn forget_whole(conn: &Connection, keep_text: bool) -> Result<()> {
    conn.execute_batch(match keep_text {
        true => "DELETE FROM messages WHERE body IS NOT NULL AND text = '';
                 UPDATE messages SET body = NULL WHERE body IS NOT NULL;",
        false => "DELETE FROM messages;",
    })?;
    conn.execute_batch(
        "UPDATE chats SET have_from = NULL, have_to = NULL;
         DELETE FROM state WHERE key IN ('sync_base', 'synced_to');",
    )?;
    Ok(())
}

/// How many messages are held whole.
pub fn whole_count(conn: &Connection) -> u64 {
    conn.query_row("SELECT count(*) FROM messages WHERE body IS NOT NULL", [], |row| {
        row.get::<_, i64>(0)
    })
    .unwrap_or(0) as u64
}

// ----- search ----------------------------------------------------------

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

/// The words someone typed, as the index sees words: runs of letters and
/// digits, in lower case.
fn words(query: &str) -> Vec<String> {
    query
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| !w.is_empty())
        .map(str::to_lowercase)
        .collect()
}

/// What someone typed, as a full-text query: every word must appear, and
/// each matches from its start, so "tomor" finds "tomorrow".
fn match_expression(query: &str) -> Option<String> {
    let words: Vec<String> = words(query).iter().map(|w| format!("\"{w}\"*")).collect();
    (!words.is_empty()).then(|| words.join(" "))
}

/// Whether the index turned a query down because one typed word is
/// several words to it. That happens in writing where marks sit inside
/// words (Hindi, Thai, Arabic with its vowels written): the index breaks
/// a word at each mark, and it keeps no record of word order, so it
/// cannot look for the pieces side by side.
fn split_a_word(error: &rusqlite::Error) -> bool {
    error.to_string().contains("phrase queries are not supported")
}

/// The same search done the slow way, for the queries the index turns
/// down (see [`split_a_word`]): read every message and keep those whose
/// text contains each word, anywhere in it. `select` and `rest` are the
/// SQL either side of the conditions; `last` is one more value for `rest`.
fn scan<T>(
    conn: &Connection,
    select: &str,
    rest: &str,
    query: &str,
    last: Option<i64>,
    row: impl FnMut(&rusqlite::Row) -> rusqlite::Result<T>,
) -> rusqlite::Result<Vec<T>> {
    use rusqlite::types::Value;
    // Words hold only letters and digits, so nothing in one can be taken
    // for a wildcard.
    let mut values: Vec<Value> = words(query).into_iter().map(|w| Value::Text(format!("%{w}%"))).collect();
    let conditions: Vec<String> = (1..=values.len()).map(|n| format!("m.text LIKE ?{n}")).collect();
    values.extend(last.map(Value::Integer));
    let sql = format!("{select} WHERE {} {rest}", conditions.join(" AND "));
    let mut statement = conn.prepare(&sql)?;
    statement.query_map(rusqlite::params_from_iter(values), row)?.collect()
}

fn hit(row: &rusqlite::Row) -> rusqlite::Result<Hit> {
    Ok(Hit {
        chat: row.get(0)?,
        guid: unpack(row.get_ref(1)?),
        date: row.get(2)?,
        from_me: row.get(3)?,
        sender: row.get(4)?,
        text: row.get(5)?,
    })
}

/// Messages containing every word of `query`, newest first.
pub fn search(conn: &Connection, query: &str, limit: usize) -> Result<Vec<Hit>> {
    let Some(expression) = match_expression(query) else {
        return Ok(Vec::new());
    };
    let indexed = || -> rusqlite::Result<Vec<Hit>> {
        let mut statement = conn.prepare_cached(
            "SELECT c.guid, m.guid, m.date, m.from_me, m.sender, m.text
             FROM messages_fts
             JOIN messages m ON m.id = messages_fts.rowid
             JOIN chats c ON c.id = m.chat
             WHERE messages_fts MATCH ?1
             ORDER BY m.date DESC
             LIMIT ?2",
        )?;
        statement.query_map(params![expression, limit as i64], hit)?.collect()
    };
    match indexed() {
        Err(e) if split_a_word(&e) => Ok(scan(
            conn,
            "SELECT c.guid, m.guid, m.date, m.from_me, m.sender, m.text
             FROM messages m JOIN chats c ON c.id = m.chat",
            &format!("ORDER BY m.date DESC LIMIT ?{}", words(query).len() + 1),
            query,
            Some(limit as i64),
            hit,
        )?),
        found => Ok(found?),
    }
}

/// One conversation's share of a search: how many of its messages match,
/// and the newest of them, which is where opening it should land.
#[derive(Debug, Clone, PartialEq)]
pub struct Mention {
    pub chat: String,
    pub count: u64,
    pub date: i64,
    pub guid: String,
}

fn mention(row: &rusqlite::Row) -> rusqlite::Result<Mention> {
    Ok(Mention {
        chat: row.get(0)?,
        count: row.get::<_, i64>(1)? as u64,
        date: row.get(2)?,
        guid: unpack(row.get_ref(3)?),
    })
}

/// Every conversation with messages containing the words of `query`, the
/// one with the most first.
pub fn mentions(conn: &Connection, query: &str) -> Result<Vec<Mention>> {
    let Some(expression) = match_expression(query) else {
        return Ok(Vec::new());
    };
    // With a single max() in the query, SQLite takes the other plain
    // columns (here the message's id) from the row that holds the maximum.
    const GROUPED: &str = "GROUP BY m.chat ORDER BY count(*) DESC, max(m.date) DESC";
    let indexed = || -> rusqlite::Result<Vec<Mention>> {
        let mut statement = conn.prepare_cached(&format!(
            "SELECT c.guid, count(*), max(m.date), m.guid
             FROM messages_fts
             JOIN messages m ON m.id = messages_fts.rowid
             JOIN chats c ON c.id = m.chat
             WHERE messages_fts MATCH ?1
             {GROUPED}"
        ))?;
        statement.query_map([&expression], mention)?.collect()
    };
    match indexed() {
        Err(e) if split_a_word(&e) => Ok(scan(
            conn,
            "SELECT c.guid, count(*), max(m.date), m.guid
             FROM messages m JOIN chats c ON c.id = m.chat",
            GROUPED,
            query,
            None,
            mention,
        )?),
        found => Ok(found?),
    }
}

/// How many messages contain every word of `query`.
pub fn count_matching(conn: &Connection, query: &str) -> Result<u64> {
    let Some(expression) = match_expression(query) else {
        return Ok(0);
    };
    let indexed = conn.query_row(
        "SELECT count(*) FROM messages_fts WHERE messages_fts MATCH ?1",
        [expression],
        |row| row.get::<_, i64>(0),
    );
    let count = match indexed {
        Err(e) if split_a_word(&e) => {
            scan(conn, "SELECT count(*) FROM messages m", "", query, None, |row| row.get::<_, i64>(0))?
                .first()
                .copied()
                .unwrap_or(0)
        }
        counted => counted?,
    };
    Ok(count as u64)
}

/// Bytes the database takes on disk, including its write-ahead file.
pub fn size_on_disk() -> u64 {
    ["", "-wal", "-shm"]
        .iter()
        .filter_map(|suffix| {
            let mut name = path().into_os_string();
            name.push(suffix);
            std::fs::metadata(Path::new(&name)).ok()
        })
        .map(|meta| meta.len())
        .sum()
}

// ----- bringing in the old text index ------------------------------------

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

/// What bringing the old index across found.
#[derive(Debug, PartialEq)]
pub struct Imported {
    /// Lines read from the old index.
    pub read: u64,
    /// Messages that were not in the database before.
    pub added: u64,
    /// Lines whose message is not in the database afterwards. Anything
    /// but zero means the old index must be kept.
    pub missing: u64,
}

/// Copy the old one-line-per-message index (`messages.tsv` and `chats.txt`
/// in `old`) into the database, then check every message arrived.
pub fn import_text_index(conn: &mut Connection, old: &Path) -> Result<Imported> {
    let chats: Vec<String> = std::fs::read_to_string(old.join("chats.txt"))
        .unwrap_or_default()
        .lines()
        .map(str::to_string)
        .collect();
    let data = std::fs::read_to_string(old.join("messages.tsv"))
        .context("couldn't read the old search index")?;
    let before = message_count(conn);
    let mut read = 0;
    let tx = conn.transaction()?;
    {
        let mut numbers: Vec<Option<i64>> = vec![None; chats.len()];
        let mut insert = tx.prepare(
            "INSERT INTO messages(guid, chat, date, from_me, sender, text)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)
             ON CONFLICT(guid) DO NOTHING",
        )?;
        for line in data.lines() {
            let mut fields = line.splitn(6, '\t');
            let (Some(date), Some(chat), Some(who), Some(sender), Some(guid), Some(text)) = (
                fields.next().and_then(|d| d.parse::<i64>().ok()),
                fields.next().and_then(|c| c.parse::<usize>().ok()),
                fields.next(),
                fields.next(),
                fields.next(),
                fields.next(),
            ) else {
                continue; // A line cut short by an interrupted write.
            };
            let Some(chat_guid) = chats.get(chat) else {
                continue;
            };
            let number = match numbers[chat] {
                Some(n) => n,
                None => {
                    let n = chat_number(&tx, chat_guid)?;
                    numbers[chat] = Some(n);
                    n
                }
            };
            read += 1;
            // The old index could hold a message twice; the first wins.
            insert.execute(params![
                pack(&unescape(guid)),
                number,
                date,
                who == "m",
                unescape(sender),
                unescape(text)
            ])?;
        }
    }
    tx.commit()?;
    // Go through the old index once more and confirm each message can be
    // found, so the caller knows it is safe to remove.
    let mut missing = 0;
    {
        let mut find = conn.prepare("SELECT 1 FROM messages WHERE guid = ?1")?;
        for line in data.lines() {
            let mut fields = line.splitn(6, '\t');
            let (Some(_), Some(chat), Some(_), Some(_), Some(guid), Some(_)) = (
                fields.next().and_then(|d| d.parse::<i64>().ok()),
                fields.next().and_then(|c| c.parse::<usize>().ok()),
                fields.next(),
                fields.next(),
                fields.next(),
                fields.next(),
            ) else {
                continue;
            };
            if chats.get(chat).is_none() {
                continue;
            }
            if !find.exists([pack(&unescape(guid))])? {
                missing += 1;
            }
        }
    }
    Ok(Imported {
        read,
        added: message_count(conn) - before,
        missing,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn memory() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        migrate(&conn).unwrap();
        conn
    }

    fn row<'a>(guid: &'a str, chat: &'a str, date: i64, text: &'a str) -> Row<'a> {
        Row {
            guid,
            chat,
            date,
            from_me: false,
            sender: "+15550100001",
            text,
        }
    }

    #[test]
    fn finds_words_by_their_start_newest_first() {
        let mut conn = memory();
        let added = put(
            &mut conn,
            &[
                row("a", "chat1", 10, "See you tomorrow at the lake"),
                row("b", "chat1", 30, "Tomorrow works for me"),
                row("c", "chat2", 20, "Nothing to do with it"),
            ],
        )
        .unwrap();
        assert_eq!(added, 3);
        let hits = search(&conn, "tomor", 10).unwrap();
        assert_eq!(hits.iter().map(|h| h.guid.as_str()).collect::<Vec<_>>(), ["b", "a"]);
        assert_eq!(hits[0].chat, "chat1");
        // Every word has to be there, in any order and any case.
        assert_eq!(search(&conn, "LAKE see", 10).unwrap().len(), 1);
        assert_eq!(search(&conn, "lake works", 10).unwrap().len(), 0);
        // Accents do not get in the way; punctuation is not a word.
        put(&mut conn, &[row("d", "chat2", 40, "Café at 7?")]).unwrap();
        assert_eq!(search(&conn, "cafe", 10).unwrap().len(), 1);
        assert!(search(&conn, "?!", 10).unwrap().is_empty());
        assert_eq!(count_matching(&conn, "tomorrow").unwrap(), 2);
    }

    #[test]
    fn finds_words_the_index_breaks_apart() {
        // Marks inside these words split them in the index, which then
        // refuses to look for the pieces together. They must still be found.
        let mut conn = memory();
        put(
            &mut conn,
            &[
                row("h", "chat1", 10, "नमस्ते दोस्त"),
                row("t", "chat1", 20, "สวัสดี ครับ"),
                row("a", "chat2", 30, "مَرْحَبًا يا صديقي"),
                row("e", "chat2", 40, "hello दोस्त, 100% sure"),
                row("x", "chat3", 50, "nothing like them"),
            ],
        )
        .unwrap();
        for (query, guids) in [
            ("दोस्त", vec!["e", "h"]),
            ("สวัสดี", vec!["t"]),
            ("ครับ", vec!["t"]),
            ("مَرْحَبًا", vec!["a"]),
            // Mixed with a word the index handles, and in any case.
            ("HELLO दोस्त", vec!["e"]),
            ("दोस्त nowhere", vec![]),
        ] {
            let found = search(&conn, query, 10).unwrap();
            assert_eq!(found.iter().map(|h| h.guid.as_str()).collect::<Vec<_>>(), guids, "{query}");
            assert_eq!(count_matching(&conn, query).unwrap(), guids.len() as u64, "{query}");
        }
        assert_eq!(search(&conn, "दोस्त", 1).unwrap().len(), 1, "the limit still applies");
        let by_chat = mentions(&conn, "दोस्त").unwrap();
        assert_eq!(by_chat.len(), 2);
        assert_eq!((by_chat[0].chat.as_str(), by_chat[0].guid.as_str()), ("chat2", "e"));
    }

    #[test]
    fn says_which_conversations_mention_the_words() {
        let mut conn = memory();
        put(
            &mut conn,
            &[
                row("a", "chat1", 10, "Lunch tomorrow?"),
                row("b", "chat2", 20, "Tomorrow is fine"),
                row("c", "chat2", 40, "See you tomorrow then"),
                row("d", "chat2", 30, "tomorrow at noon"),
                row("e", "chat3", 50, "Nothing relevant"),
            ],
        )
        .unwrap();
        let found = mentions(&conn, "tomorrow").unwrap();
        // The conversation with the most comes first, and each carries its
        // newest match, which is where opening it lands.
        assert_eq!(found.len(), 2);
        assert_eq!((found[0].chat.as_str(), found[0].count), ("chat2", 3));
        assert_eq!((found[0].guid.as_str(), found[0].date), ("c", 40));
        assert_eq!((found[1].chat.as_str(), found[1].count, found[1].guid.as_str()), ("chat1", 1, "a"));
        assert!(mentions(&conn, "nowhere").unwrap().is_empty());
        assert!(mentions(&conn, "  ").unwrap().is_empty());
    }

    #[test]
    fn an_edit_or_removal_reaches_search() {
        let mut conn = memory();
        put(&mut conn, &[row("a", "chat1", 10, "meet at noon")]).unwrap();
        // The same message again with new text is an edit, not a new one.
        let added = put(&mut conn, &[row("a", "chat1", 10, "meet at three")]).unwrap();
        assert_eq!(added, 0);
        assert_eq!(message_count(&conn), 1);
        assert!(search(&conn, "noon", 10).unwrap().is_empty());
        assert_eq!(search(&conn, "three", 10).unwrap().len(), 1);
        remove(&conn, "a");
        assert!(search(&conn, "three", 10).unwrap().is_empty());
    }

    #[test]
    fn ids_survive_being_packed() {
        use rusqlite::types::{Value, ValueRef};
        let standard = "A1B2C3D4-0000-4000-8000-123456789ABC";
        let packed = pack(standard);
        assert!(matches!(&packed, Value::Blob(bytes) if bytes.len() == 16));
        assert_eq!(unpack(ValueRef::from(&packed)), standard);
        // Anything not in exactly that shape is kept as written.
        for odd in ["a1b2c3d4-0000-4000-8000-123456789abc", "temp-12-3", "", "p:0/ABC"] {
            let packed = pack(odd);
            assert!(matches!(&packed, Value::Text(_)), "{odd}");
            assert_eq!(unpack(ValueRef::from(&packed)), odd);
        }
        // And both kinds work as the id of a stored message.
        let mut conn = memory();
        put(&mut conn, &[row(standard, "c", 1, "packed one"), row("temp-12-3", "c", 2, "odd one")]).unwrap();
        assert_eq!(search(&conn, "packed", 5).unwrap()[0].guid, standard);
        assert_eq!(search(&conn, "odd", 5).unwrap()[0].guid, "temp-12-3");
        assert_eq!(put(&mut conn, &[row(standard, "c", 1, "packed one")]).unwrap(), 0);
        remove(&conn, standard);
        assert_eq!(message_count(&conn), 1);
    }

    #[test]
    fn trims_by_date_and_by_count() {
        let mut conn = memory();
        let rows: Vec<(String, i64)> = (1..=10).map(|i| (format!("m{i}"), i * 100)).collect();
        let batch: Vec<Row> = rows.iter().map(|(g, d)| row(g, "c", *d, "word")).collect();
        put(&mut conn, &batch).unwrap();
        assert_eq!(oldest_date(&conn), Some(100));
        // Everything before 350 goes: m1, m2, m3.
        assert_eq!(delete_before(&conn, 350), 3);
        assert_eq!(oldest_date(&conn), Some(400));
        // Then the two oldest of what is left: m4, m5.
        assert_eq!(delete_oldest(&conn, 2), 2);
        assert_eq!(oldest_date(&conn), Some(600));
        // Search no longer finds what was removed.
        assert_eq!(count_matching(&conn, "word").unwrap(), 5);
        assert_eq!(message_count(&conn), 5);
    }

    #[test]
    fn keeps_read_markers_and_single_values() {
        let conn = memory();
        set_seen(&conn, "chat1", 100).unwrap();
        set_seen(&conn, "chat1", 250).unwrap();
        set_seen(&conn, "chat2", 5).unwrap();
        let all = seen(&conn);
        assert_eq!((all["chat1"], all["chat2"]), (250, 5));
        forget_seen(&conn, "chat2");
        assert_eq!(seen(&conn).len(), 1);
        assert_eq!(state(&conn, "missing"), None);
        set_state(&conn, "k", "1").unwrap();
        set_state(&conn, "k", "2").unwrap();
        assert_eq!(state(&conn, "k").as_deref(), Some("2"));
    }

    #[test]
    fn imports_the_old_text_index() {
        let dir = std::env::temp_dir().join(format!("fizz-store-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("chats.txt"), "chatA\nchatB\n").unwrap();
        std::fs::write(
            dir.join("messages.tsv"),
            "30\t0\tt\t+15550100001\tg1\thello\\nthere\n\
             20\t1\tm\t\tg2\ta\\ttab\n\
             10\t0\tt\t+15550100001\tg1\tduplicate of g1\n\
             broken line\n",
        )
        .unwrap();
        let mut conn = memory();
        let imported = import_text_index(&mut conn, &dir).unwrap();
        assert_eq!(imported, Imported { read: 3, added: 2, missing: 0 });
        // Doing it again adds nothing and still finds everything.
        let again = import_text_index(&mut conn, &dir).unwrap();
        assert_eq!(again, Imported { read: 3, added: 0, missing: 0 });
        let hits = search(&conn, "hello", 10).unwrap();
        assert_eq!(hits[0].text, "hello\nthere");
        assert_eq!(hits[0].chat, "chatA");
        let mine = search(&conn, "tab", 10).unwrap();
        assert!(mine[0].from_me && mine[0].chat == "chatB" && mine[0].text == "a\ttab");
        std::fs::remove_dir_all(&dir).unwrap();
    }
    #[test]
    fn conversations_are_filed_and_taken_out() {
        let conn = memory();
        file(&conn, "chat1", Some("Stats")).unwrap();
        file(&conn, "chat2", Some("Stats")).unwrap();
        file(&conn, "chat1", Some("Codes")).unwrap();
        let held = filed(&conn);
        assert_eq!(held.len(), 2);
        assert_eq!((held["chat1"].as_str(), held["chat2"].as_str()), ("Codes", "Stats"));
        file(&conn, "chat1", None).unwrap();
        assert_eq!(filed(&conn).len(), 1);
    }

    // ----- whole messages ------------------------------------------------

    use crate::model::{Attachment, ChatRef, Handle};

    const DAY: i64 = 24 * 3600 * 1000;
    /// "Now" for these tests; messages are dated in the days before it.
    const NOW: i64 = 1_800_000_000_000;

    fn said(guid: &str, days_ago: i64, text: &str) -> Message {
        Message {
            guid: guid.into(),
            text: Some(text.into()),
            date_created: NOW - days_ago * DAY,
            handle: Some(Handle { address: "+15550100001".into(), service: Some("iMessage".into()) }),
            ..Message::default()
        }
    }

    fn in_chat(mut message: Message, chat: &str) -> Message {
        message.chats = vec![ChatRef { guid: chat.into(), ..ChatRef::default() }];
        message
    }

    fn guids(messages: &[Message]) -> Vec<&str> {
        messages.iter().map(|m| m.guid.as_str()).collect()
    }

    #[test]
    fn a_message_comes_back_as_it_went_in() {
        let mut conn = memory();
        let mut photo = said("p", 2, "Look at this");
        photo.attachments = vec![Attachment {
            guid: "att-1".into(),
            mime_type: Some("image/jpeg".into()),
            transfer_name: Some("IMG_1.jpeg".into()),
            total_bytes: Some(2048),
            ..Attachment::default()
        }];
        photo.date_delivered = Some(NOW - 2 * DAY + 5);
        photo.is_delivered = true;
        photo.thread_originator_guid = Some("first".into());
        let mut tapback = said("t", 1, "Loved \u{201c}Look at this\u{201d}");
        tapback.is_from_me = true;
        tapback.handle = None;
        tapback.associated_message_guid = Some("p:0/p".into());
        tapback.associated_message_type = Some(serde_json::json!("love"));
        let mut link = said("l", 3, "https://example.org/trail");
        // Apple's archive: objects that point at each other by number.
        link.payload_data = Some(serde_json::json!([{ "$objects": [
            "$null",
            { "title": { "UID": 2 }, "originalURL": { "UID": 3 }, "padding": "x".repeat(4000) },
            "Trail conditions",
            "https://example.org/trail",
        ]}]));
        let mut wordless = said("w", 4, "");
        wordless.text = None;
        wordless.attributed_body = Some(serde_json::json!([{ "string": "Only in the attributed form" }]));

        let page = [tapback.clone(), photo.clone(), link.clone(), wordless.clone()];
        keep_page(&mut conn, "chat1", &page, Shape::Latest { limit: 50 }, NOW).unwrap();
        let back = super::page(&conn, "chat1", None, 50).messages;
        assert_eq!(guids(&back), ["t", "p", "l", "w"], "newest first");

        let (t, p, l, w) = (&back[0], &back[1], &back[2], &back[3]);
        assert_eq!(p.body(), "Look at this");
        assert_eq!(p.date_created, photo.date_created);
        assert_eq!(p.date_delivered, photo.date_delivered);
        assert!(p.is_delivered && !p.is_from_me);
        assert_eq!(p.handle.as_ref().unwrap().service.as_deref(), Some("iMessage"));
        assert_eq!(p.attachments.len(), 1);
        assert_eq!(p.attachments[0].guid, "att-1");
        assert_eq!(p.attachments[0].total_bytes, Some(2048));
        assert_eq!(p.thread_originator_guid.as_deref(), Some("first"));
        assert!(t.is_from_me && t.reaction().is_some());
        assert_eq!(t.reaction_target(), Some("p"));
        assert_eq!(l.link_title().as_deref(), Some("Trail conditions"));
        assert_eq!(w.body(), "Only in the attributed form");

        // A tapback is kept but not found by its words; the rest are.
        assert_eq!(search(&conn, "loved", 10).unwrap().len(), 0);
        assert_eq!(search(&conn, "look", 10).unwrap().len(), 1);
        assert_eq!(search(&conn, "attributed", 10).unwrap().len(), 1);
        // The link's four kilobytes of archive are not what is kept.
        let size: i64 = conn
            .query_row("SELECT length(body) FROM messages WHERE guid = 'l'", [], |r| r.get(0))
            .unwrap();
        assert!(size < 400, "{size} bytes");
    }

    #[test]
    fn nothing_is_read_from_a_conversation_never_fetched() {
        let mut conn = memory();
        // Known for search only: text, not the whole message.
        put(&mut conn, &[row("a", "chat1", NOW - DAY, "Hello there")]).unwrap();
        let local = super::page(&conn, "chat1", None, 50);
        assert!(local.messages.is_empty() && !local.complete);
        assert!(super::page(&conn, "never-heard-of", None, 50).messages.is_empty());
    }

    #[test]
    fn pages_join_up_going_back() {
        let mut conn = memory();
        let newest = [said("f", 1, "six"), said("e", 2, "five"), said("d", 3, "four")];
        let stored = keep_page(&mut conn, "chat1", &newest, Shape::Latest { limit: 3 }, NOW).unwrap();
        assert!(!stored.joined, "nothing was held to join");
        let local = super::page(&conn, "chat1", None, 3);
        assert_eq!(guids(&local.messages), ["f", "e", "d"]);
        assert!(local.complete && !local.reaches_start);

        // Older than that is not held yet: the store says so, not "none".
        let before = NOW - 3 * DAY + 1;
        let local = super::page(&conn, "chat1", Some(before), 3);
        assert_eq!(guids(&local.messages), ["d"], "only the one it shares");
        assert!(!local.complete);

        // The next page back, as the app asks for it: from one past the
        // oldest it has, so that one comes again.
        let older = [said("d", 3, "four"), said("c", 4, "three"), said("b", 5, "two")];
        keep_page(&mut conn, "chat1", &older, Shape::Older { before, limit: 3 }, NOW).unwrap();
        let local = super::page(&conn, "chat1", Some(before), 3);
        assert_eq!(guids(&local.messages), ["d", "c", "b"]);
        assert!(local.complete && !local.reaches_start);

        // A short page is the start of the conversation.
        let before = NOW - 5 * DAY + 1;
        let first = [said("b", 5, "two"), said("a", 6, "one")];
        keep_page(&mut conn, "chat1", &first, Shape::Older { before, limit: 3 }, NOW).unwrap();
        let local = super::page(&conn, "chat1", Some(before), 3);
        assert_eq!(guids(&local.messages), ["b", "a"]);
        assert!(local.complete && local.reaches_start);
        assert_eq!(covered(&conn, "chat1"), Some((BEGINNING, NOW)));
        // And all of it reads back in one go.
        assert_eq!(guids(&super::page(&conn, "chat1", None, 50).messages), ["f", "e", "d", "c", "b", "a"]);
    }

    #[test]
    fn a_hole_is_never_read_across() {
        let mut conn = memory();
        let old = [said("c", 30, "three"), said("b", 31, "two"), said("a", 32, "one")];
        keep_page(&mut conn, "chat1", &old, Shape::Latest { limit: 3 }, NOW - 30 * DAY).unwrap();
        // A month on, the newest page is all new: whatever was said in
        // between was never fetched.
        let new = [said("z", 1, "ten"), said("y", 2, "nine"), said("x", 3, "eight")];
        let stored = keep_page(&mut conn, "chat1", &new, Shape::Latest { limit: 3 }, NOW).unwrap();
        assert!(!stored.joined);
        assert!(stored.gone.is_empty(), "the old ones are out of sight, not deleted");
        let local = super::page(&conn, "chat1", None, 50);
        assert_eq!(guids(&local.messages), ["z", "y", "x"], "not the ones beyond the hole");
        assert!(!local.complete);

        // A newest page that overlaps what is held does join.
        let next = [said("zz", 0, "eleven"), said("z", 1, "ten"), said("y", 2, "nine")];
        let stored = keep_page(&mut conn, "chat1", &next, Shape::Latest { limit: 3 }, NOW + 1000).unwrap();
        assert!(stored.joined);
        assert_eq!(guids(&super::page(&conn, "chat1", None, 50).messages), ["zz", "z", "y", "x"]);
    }

    #[test]
    fn what_the_server_no_longer_has_goes() {
        let mut conn = memory();
        let page = [said("d", 1, "four"), said("c", 2, "three"), said("b", 3, "two"), said("a", 4, "one")];
        keep_page(&mut conn, "chat1", &page, Shape::Latest { limit: 4 }, NOW).unwrap();
        // Text known for search only, in the same stretch, is left alone:
        // only whole messages are vouched for by a page.
        put(&mut conn, &[row("s", "chat1", NOW - 2 * DAY - 5, "search only")]).unwrap();
        // Something that arrived seconds ago, after this page was put together.
        keep(&mut conn, &[in_chat(said("live", 0, "just now"), "chat1")]).unwrap();

        // "c" was deleted on the phone; "b" was edited.
        let mut edited = said("b", 3, "two, corrected");
        edited.date_edited = Some(NOW - DAY);
        let again = [said("d", 1, "four"), edited, said("a", 4, "one")];
        let stored = keep_page(&mut conn, "chat1", &again, Shape::Latest { limit: 4 }, NOW).unwrap();
        assert_eq!(stored.gone, ["c"]);
        let local = super::page(&conn, "chat1", None, 50);
        assert_eq!(guids(&local.messages), ["live", "d", "b", "a"]);
        assert_eq!(local.messages[2].body(), "two, corrected");
        assert!(local.messages[2].date_edited.is_some());
        assert_eq!(search(&conn, "three", 10).unwrap().len(), 0);
        assert_eq!(search(&conn, "corrected", 10).unwrap().len(), 1);
        assert_eq!(search(&conn, "search", 10).unwrap().len(), 1);
    }

    #[test]
    fn what_arrives_is_held_for_every_conversation() {
        let mut conn = memory();
        // Fizz began taking everything in three days ago.
        start_sync(&conn, NOW - 3 * DAY).unwrap();
        keep(
            &mut conn,
            &[
                in_chat(said("a", 2, "to one"), "chat1"),
                in_chat(said("b", 1, "to another"), "chat2"),
                in_chat(said("c", 1, "to one again"), "chat1"),
            ],
        )
        .unwrap();
        set_synced_to(&conn, NOW - DAY).unwrap();
        set_synced_to(&conn, NOW - 2 * DAY).unwrap();
        assert_eq!(synced(&conn), Some((NOW - 3 * DAY, NOW - DAY)), "never backwards");

        // A conversation never opened can be read back to that point...
        let local = super::page(&conn, "chat1", None, 50);
        assert_eq!(guids(&local.messages), ["c", "a"]);
        assert!(!local.complete, "...but no further is known");

        // Its newest page from the server then joins on, and reaches back.
        let page = [said("c", 1, "to one again"), said("a", 2, "to one"), said("z", 9, "long before")];
        let stored = keep_page(&mut conn, "chat1", &page, Shape::Latest { limit: 3 }, NOW).unwrap();
        assert!(stored.joined);
        assert_eq!(covered(&conn, "chat1"), Some((NOW - 9 * DAY, NOW)));
        assert_eq!(covered(&conn, "chat2"), Some((NOW - 3 * DAY, NOW - DAY)));
    }

    #[test]
    fn what_arrived_beyond_a_cut_short_catch_up_is_not_read() {
        let mut conn = memory();
        start_sync(&conn, NOW - 9 * DAY).unwrap();
        keep(&mut conn, &[in_chat(said("a", 8, "before"), "chat1")]).unwrap();
        set_synced_to(&conn, NOW - 7 * DAY).unwrap();
        // Catching up a week later got no further than this, but a message
        // arriving live in the meantime was stored all the same.
        keep(&mut conn, &[in_chat(said("z", 0, "just now"), "chat1")]).unwrap();
        assert_eq!(guids(&super::page(&conn, "chat1", None, 50).messages), ["a"]);
        // Once the catch-up has passed it, it is part of the run.
        set_synced_to(&conn, NOW).unwrap();
        assert_eq!(guids(&super::page(&conn, "chat1", None, 50).messages), ["z", "a"]);
    }

    #[test]
    fn a_conversation_under_two_names_is_not_vouched_for() {
        let mut conn = memory();
        keep(&mut conn, &[in_chat(said("a", 1, "hello"), "any;-;+15550100001")]).unwrap();
        // The same message, in a page asked for under another name.
        let page = [said("a", 1, "hello"), said("b", 2, "earlier")];
        keep_page(&mut conn, "iMessage;-;+15550100001", &page, Shape::Latest { limit: 50 }, NOW).unwrap();
        let stored =
            keep_page(&mut conn, "iMessage;-;+15550100001", &page, Shape::Latest { limit: 50 }, NOW).unwrap();
        assert!(!stored.vouched && stored.gone.is_empty());
        assert_eq!(covered(&conn, "iMessage;-;+15550100001"), None);
        assert!(super::page(&conn, "iMessage;-;+15550100001", None, 50).messages.is_empty());
    }

    #[test]
    fn a_link_keeps_its_title_when_heard_of_again() {
        let mut conn = memory();
        let mut link = said("l", 1, "https://example.org");
        link.payload_data = Some(serde_json::json!([{ "$objects": [
            { "title": { "UID": 1 }, "originalURL": { "UID": 1 } }, "Example" ]}]));
        keep_page(&mut conn, "chat1", &[link], Shape::Latest { limit: 50 }, NOW).unwrap();
        // The live update for the same message carries no preview.
        let mut read = in_chat(said("l", 1, "https://example.org"), "chat1");
        read.date_read = Some(NOW);
        keep(&mut conn, &[read]).unwrap();
        let back = &super::page(&conn, "chat1", None, 50).messages[0];
        assert_eq!(back.link_title().as_deref(), Some("Example"));
        assert_eq!(back.date_read, Some(NOW));
    }

    #[test]
    fn text_kept_for_search_becomes_a_whole_message() {
        let mut conn = memory();
        put(&mut conn, &[row("a", "chat1", NOW - DAY, "See you tomorrow")]).unwrap();
        assert_eq!(whole_count(&conn), 0);
        let added = keep(&mut conn, &[in_chat(said("a", 1, "See you tomorrow"), "chat1")]).unwrap();
        assert_eq!(added, 0, "it was already counted");
        assert_eq!((message_count(&conn), whole_count(&conn)), (1, 1));
        assert_eq!(search(&conn, "tomorrow", 10).unwrap().len(), 1);
        // And the other way round does not undo it.
        put(&mut conn, &[row("a", "chat1", NOW - DAY, "See you tomorrow")]).unwrap();
        assert_eq!(whole_count(&conn), 1);
        // An unsent message stays, as the conversation shows it, but is
        // no longer found by what it said.
        let mut unsent = in_chat(said("a", 1, "See you tomorrow"), "chat1");
        unsent.date_retracted = Some(NOW);
        keep(&mut conn, &[unsent]).unwrap();
        assert_eq!(whole_count(&conn), 1);
        assert_eq!(search(&conn, "tomorrow", 10).unwrap().len(), 0);
    }

    #[test]
    fn trimming_shortens_what_is_vouched_for() {
        let mut conn = memory();
        let page = [said("c", 1, "three"), said("b", 10, "two"), said("a", 20, "one")];
        keep_page(&mut conn, "chat1", &page, Shape::Latest { limit: 50 }, NOW).unwrap();
        start_sync(&conn, NOW - 15 * DAY).unwrap();
        set_synced_to(&conn, NOW - DAY).unwrap();
        assert_eq!(covered(&conn, "chat1"), Some((BEGINNING, NOW)));
        assert_eq!(delete_before(&conn, NOW - 5 * DAY), 2);
        assert_eq!(covered(&conn, "chat1"), Some((NOW - 5 * DAY, NOW)));
        assert_eq!(synced(&conn).map(|s| s.0), Some(NOW - 5 * DAY));
        let local = super::page(&conn, "chat1", None, 50);
        assert_eq!(guids(&local.messages), ["c"]);
        assert!(!local.complete, "the start is no longer held");
        // A deleted conversation takes its messages with it.
        put(&mut conn, &[row("s", "chat1", NOW, "for search")]).unwrap();
        forget_chat(&conn, "chat1");
        assert_eq!((message_count(&conn), covered(&conn, "chat1").map(|c| c.0)), (0, Some(NOW - 5 * DAY)));
        assert!(super::page(&conn, "chat1", None, 50).messages.is_empty());
        // Turning the keeping of conversations off leaves search its text.
        keep(&mut conn, &[in_chat(said("w", 1, "kept whole"), "chat2"), in_chat(said("t", 1, ""), "chat2")]).unwrap();
        forget_whole(&conn, true).unwrap();
        assert_eq!((whole_count(&conn), message_count(&conn)), (0, 1));
        assert_eq!(search(&conn, "whole", 10).unwrap().len(), 1);
        assert_eq!((covered(&conn, "chat1"), synced(&conn)), (None, None));
        start_sync(&conn, NOW).unwrap();
        // Emptying the store forgets all of it.
        clear_messages(&conn).unwrap();
        assert_eq!((covered(&conn, "chat1"), synced(&conn)), (None, None));
    }

}
