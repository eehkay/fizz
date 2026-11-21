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
const SCHEMA_VERSION: i64 = 1;

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
         INSERT INTO messages_fts(messages_fts) VALUES ('rebuild');
         VACUUM;",
    )?;
    Ok(())
}

/// Remove messages older than `date` (milliseconds). Returns how many.
pub fn delete_before(conn: &Connection, date: i64) -> u64 {
    conn.execute("DELETE FROM messages WHERE date < ?1", [date])
        .unwrap_or(0) as u64
}

/// Remove the `count` oldest messages. Returns how many went.
pub fn delete_oldest(conn: &Connection, count: u64) -> u64 {
    conn.execute(
        "DELETE FROM messages WHERE id IN
         (SELECT id FROM messages ORDER BY date ASC LIMIT ?1)",
        [count as i64],
    )
    .unwrap_or(0) as u64
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

/// What someone typed, as a full-text query: every word must appear, and
/// each matches from its start, so "tomor" finds "tomorrow".
fn match_expression(query: &str) -> Option<String> {
    let words: Vec<String> = query
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| !w.is_empty())
        .map(|w| format!("\"{}\"*", w.to_lowercase()))
        .collect();
    (!words.is_empty()).then(|| words.join(" "))
}

/// Messages containing every word of `query`, newest first.
pub fn search(conn: &Connection, query: &str, limit: usize) -> Result<Vec<Hit>> {
    let Some(expression) = match_expression(query) else {
        return Ok(Vec::new());
    };
    let mut statement = conn.prepare_cached(
        "SELECT c.guid, m.guid, m.date, m.from_me, m.sender, m.text
         FROM messages_fts
         JOIN messages m ON m.id = messages_fts.rowid
         JOIN chats c ON c.id = m.chat
         WHERE messages_fts MATCH ?1
         ORDER BY m.date DESC
         LIMIT ?2",
    )?;
    let hits = statement
        .query_map(params![expression, limit as i64], |row| {
            Ok(Hit {
                chat: row.get(0)?,
                guid: unpack(row.get_ref(1)?),
                date: row.get(2)?,
                from_me: row.get(3)?,
                sender: row.get(4)?,
                text: row.get(5)?,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(hits)
}

/// How many messages contain every word of `query`.
pub fn count_matching(conn: &Connection, query: &str) -> Result<u64> {
    let Some(expression) = match_expression(query) else {
        return Ok(0);
    };
    Ok(conn.query_row(
        "SELECT count(*) FROM messages_fts WHERE messages_fts MATCH ?1",
        [expression],
        |row| row.get::<_, i64>(0),
    )? as u64)
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
}
