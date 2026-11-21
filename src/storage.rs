//! Keeping what Fizz stores on disk within limits the user sets.
//!
//! There are three stores, each with its own limit (see the settings):
//! the database of message text for search, the picture cache (thumbnails
//! and GIFs), and the file cache (things opened or saved, and videos
//! fetched for a still frame). This module trims the two caches and
//! reports on all three; the database trims itself in `index.rs`.

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use crate::config::{cache_dir, settings};

const MB: u64 = 1024 * 1024;
/// Thumbnails nobody has looked at in this long go, whatever the size.
const PICTURE_AGE: Duration = Duration::from_secs(90 * 24 * 3600);
/// The log is for recent trouble, not history.
const LOG_LIMIT: u64 = 256 * 1024;

/// One thing in a cache: a file, or a folder holding one download.
#[derive(Debug, Clone, PartialEq)]
pub struct Entry {
    pub path: PathBuf,
    pub bytes: u64,
    /// When it was last used: written, or read back since.
    pub used: SystemTime,
}

fn size_and_newest(path: &Path) -> (u64, SystemTime) {
    let Ok(meta) = std::fs::metadata(path) else {
        return (0, SystemTime::UNIX_EPOCH);
    };
    let own = meta.modified().unwrap_or(SystemTime::UNIX_EPOCH);
    if !meta.is_dir() {
        return (meta.len(), own);
    }
    let Ok(children) = std::fs::read_dir(path) else {
        return (0, own);
    };
    children
        .flatten()
        .map(|child| size_and_newest(&child.path()))
        .fold((0, own), |(bytes, newest), (b, n)| (bytes + b, newest.max(n)))
}

/// Everything directly inside these folders, as entries.
pub fn entries(folders: &[PathBuf]) -> Vec<Entry> {
    folders
        .iter()
        .filter_map(|folder| std::fs::read_dir(folder).ok())
        .flatten()
        .flatten()
        .map(|item| {
            let path = item.path();
            let (bytes, used) = size_and_newest(&path);
            Entry { path, bytes, used }
        })
        .collect()
}

/// Which entries to remove: anything unused for longer than `max_age`,
/// then, if what is left is still over `cap`, the least recently used
/// until it is comfortably under (so the next few additions do not tip it
/// straight back over).
pub fn to_remove(
    mut entries: Vec<Entry>,
    now: SystemTime,
    max_age: Option<Duration>,
    cap: Option<u64>,
) -> Vec<Entry> {
    let mut removing = Vec::new();
    if let Some(max_age) = max_age {
        let (old, kept): (Vec<Entry>, Vec<Entry>) = entries.into_iter().partition(|e| {
            now.duration_since(e.used)
                .is_ok_and(|unused_for| unused_for > max_age)
        });
        removing = old;
        entries = kept;
    }
    if let Some(cap) = cap {
        let mut total: u64 = entries.iter().map(|e| e.bytes).sum();
        if total > cap {
            let target = cap / 10 * 9;
            entries.sort_by_key(|e| e.used);
            for entry in entries {
                if total <= target {
                    break;
                }
                total -= entry.bytes;
                removing.push(entry);
            }
        }
    }
    removing
}

fn remove(entry: &Entry) {
    let _ = if entry.path.is_dir() {
        std::fs::remove_dir_all(&entry.path)
    } else {
        std::fs::remove_file(&entry.path)
    };
}

/// Megabytes from the settings as bytes; zero means "no limit".
fn cap(megabytes: u64) -> Option<u64> {
    (megabytes > 0).then(|| megabytes.saturating_mul(MB))
}

fn picture_folders() -> Vec<PathBuf> {
    vec![cache_dir().join("thumbs"), cache_dir().join("gifs")]
}

fn file_folders() -> Vec<PathBuf> {
    vec![cache_dir().join("files"), cache_dir().join("outbox")]
}

/// Bring both caches within their limits, and keep the log short. Cheap
/// enough to run at startup and now and then while Fizz is open.
pub fn trim() {
    let media = &settings().media;
    let now = SystemTime::now();
    for entry in to_remove(
        entries(&picture_folders()),
        now,
        Some(PICTURE_AGE),
        cap(media.picture_cache_mb),
    ) {
        remove(&entry);
    }
    let file_age = (media.file_cache_days > 0)
        .then(|| Duration::from_secs(media.file_cache_days.saturating_mul(24 * 3600)));
    for entry in to_remove(entries(&file_folders()), now, file_age, cap(media.file_cache_mb)) {
        remove(&entry);
    }
    trim_log();
}

/// Keep only the tail of the log once it has grown.
fn trim_log() {
    let path = cache_dir().join("fizz.log");
    let Ok(meta) = std::fs::metadata(&path) else {
        return;
    };
    if meta.len() <= LOG_LIMIT {
        return;
    }
    if let Ok(text) = std::fs::read_to_string(&path) {
        let keep_from = text.len().saturating_sub((LOG_LIMIT / 4) as usize);
        // Start at a line boundary so no line is left cut in half.
        let start = text[keep_from..].find('\n').map_or(text.len(), |i| keep_from + i + 1);
        let _ = std::fs::write(&path, &text[start..]);
    }
}

/// Mark something in a cache as just used, so it is the last to be trimmed.
pub fn touch(path: &Path) {
    if let Ok(file) = std::fs::File::options().write(true).open(path) {
        let _ = file.set_modified(SystemTime::now());
    }
}

/// One line of the storage report.
pub struct Line {
    pub name: &'static str,
    pub bytes: u64,
    pub items: u64,
    /// The limit in force, in words.
    pub limit: String,
    pub place: PathBuf,
}

/// What each store is using, and its limit.
pub fn report() -> Vec<Line> {
    let s = settings();
    let megabytes = |mb: u64| match mb {
        0 => "no size limit".to_string(),
        mb => format!("up to {mb} MB"),
    };
    let pictures = entries(&picture_folders());
    let files = entries(&file_folders());
    let database_rows = crate::store::path()
        .exists()
        .then(|| crate::store::open().ok())
        .flatten()
        .map_or(0, |conn| crate::store::message_count(&conn));
    vec![
        Line {
            name: "Search text",
            bytes: crate::store::size_on_disk(),
            items: database_rows,
            limit: match (s.search.years, s.search.max_mb) {
                (0, mb) => format!("all years, {}", megabytes(mb)),
                (years, mb) => format!("last {years} years, {}", megabytes(mb)),
            },
            place: crate::store::path(),
        },
        Line {
            name: "Pictures",
            bytes: pictures.iter().map(|e| e.bytes).sum(),
            items: pictures.len() as u64,
            limit: format!("{}, unused for 90 days", megabytes(s.media.picture_cache_mb)),
            place: cache_dir().join("thumbs"),
        },
        Line {
            name: "Files",
            bytes: files.iter().map(|e| e.bytes).sum(),
            items: files.len() as u64,
            limit: match s.media.file_cache_days {
                0 => megabytes(s.media.file_cache_mb),
                days => format!("{}, unused for {days} days", megabytes(s.media.file_cache_mb)),
            },
            place: cache_dir().join("files"),
        },
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(name: &str, megabytes: u64, days_ago: u64, now: SystemTime) -> Entry {
        Entry {
            path: PathBuf::from(name),
            bytes: megabytes * MB,
            used: now - Duration::from_secs(days_ago * 24 * 3600),
        }
    }

    fn names(entries: &[Entry]) -> Vec<&str> {
        let mut names: Vec<&str> = entries.iter().filter_map(|e| e.path.to_str()).collect();
        names.sort();
        names
    }

    #[test]
    fn old_things_go_regardless_of_size() {
        let now = SystemTime::now();
        let all = vec![entry("fresh", 1, 1, now), entry("stale", 1, 20, now)];
        let gone = to_remove(all, now, Some(Duration::from_secs(14 * 24 * 3600)), Some(500 * MB));
        assert_eq!(names(&gone), ["stale"]);
    }

    #[test]
    fn over_the_limit_the_least_recently_used_go_first() {
        let now = SystemTime::now();
        let all = vec![
            entry("oldest", 40, 9, now),
            entry("older", 40, 6, now),
            entry("recent", 40, 2, now),
            entry("newest", 40, 0, now),
        ];
        // 160 MB against a 100 MB limit: trimmed to under 90, which takes
        // the two least recently used.
        let gone = to_remove(all.clone(), now, None, Some(100 * MB));
        assert_eq!(names(&gone), ["older", "oldest"]);
        // Within the limit, nothing goes.
        assert!(to_remove(all.clone(), now, None, Some(200 * MB)).is_empty());
        // No limit at all, nothing goes.
        assert!(to_remove(all, now, None, None).is_empty());
    }

    #[test]
    fn both_rules_apply_together() {
        let now = SystemTime::now();
        let all = vec![
            entry("ancient", 10, 100, now),
            entry("a", 60, 5, now),
            entry("b", 60, 1, now),
        ];
        let gone = to_remove(all, now, Some(Duration::from_secs(90 * 24 * 3600)), Some(100 * MB));
        assert_eq!(names(&gone), ["a", "ancient"]);
    }
}
