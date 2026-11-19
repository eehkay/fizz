//! Measurements for the stats screen: how long server requests take, how
//! the live connection is doing, and what the app itself is using.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// How many recent requests are kept for the latency figures.
const KEPT: usize = 60;

pub struct Sample {
    pub what: &'static str,
    pub took: Duration,
    pub ok: bool,
}

#[derive(Default)]
pub struct Metrics {
    pub samples: VecDeque<Sample>,
    pub requests: u64,
    pub failures: u64,
    /// The live (socket) connection: connected or not, once known.
    pub live: Option<bool>,
    pub live_events: u64,
    pub last_live_event: Option<Instant>,
    /// The catch-up poll.
    pub polls: u64,
    pub poll_failures: u64,
    pub last_poll: Option<Instant>,
}

pub type Shared = Arc<Mutex<Metrics>>;

/// A short name for the kind of request, from its path.
pub fn kind(path: &str) -> &'static str {
    match path {
        p if p.starts_with("/message/query") => "messages",
        p if p.starts_with("/message/text") => "send",
        p if p.starts_with("/message/attachment") => "send file",
        p if p.starts_with("/message/react") => "tapback",
        p if p.ends_with("/unsend") => "unsend",
        p if p.ends_with("/edit") => "edit",
        p if p.starts_with("/chat/query") => "chat list",
        p if p.starts_with("/chat/new") => "new chat",
        p if p.ends_with("/read") => "mark read",
        p if p.ends_with("/typing") => "typing",
        p if p.starts_with("/chat/") => "chat",
        p if p.starts_with("/contact") => "contacts",
        p if p.starts_with("/attachment") => "download",
        p if p.starts_with("/server") => "server info",
        _ => "other",
    }
}

pub fn record(metrics: &Shared, what: &'static str, started: Instant, ok: bool) {
    if let Ok(mut m) = metrics.lock() {
        m.requests += 1;
        if !ok {
            m.failures += 1;
        }
        m.samples.push_back(Sample {
            what,
            took: started.elapsed(),
            ok,
        });
        while m.samples.len() > KEPT {
            m.samples.pop_front();
        }
    }
}

/// Latency figures over the kept samples, leaving out downloads and
/// uploads, whose time is about file size rather than responsiveness.
pub struct Latency {
    pub last: Option<(Duration, &'static str)>,
    pub median: Option<Duration>,
    pub slowest: Option<(Duration, &'static str)>,
    /// Recent request times, oldest first, for a sparkline.
    pub recent: Vec<Duration>,
}

impl Metrics {
    pub fn latency(&self) -> Latency {
        let quick: Vec<&Sample> = self
            .samples
            .iter()
            .filter(|s| s.ok && !matches!(s.what, "download" | "send file"))
            .collect();
        let mut sorted: Vec<Duration> = quick.iter().map(|s| s.took).collect();
        sorted.sort();
        Latency {
            last: quick.last().map(|s| (s.took, s.what)),
            median: sorted.get(sorted.len() / 2).copied().filter(|_| !sorted.is_empty()),
            slowest: quick.iter().max_by_key(|s| s.took).map(|s| (s.took, s.what)),
            recent: quick.iter().rev().take(32).rev().map(|s| s.took).collect(),
        }
    }
}

/// What this process is using right now.
#[derive(Default, Clone, Copy)]
pub struct Usage {
    pub memory_bytes: Option<u64>,
    /// CPU time used so far, for working out a percentage between samples.
    pub cpu_time: Option<Duration>,
    pub threads: Option<u64>,
}

/// Read usage from the operating system. Linux has it in /proc; elsewhere
/// only memory is reported, through `ps`.
pub fn usage() -> Usage {
    if let Ok(stat) = std::fs::read_to_string("/proc/self/stat") {
        // The command name is in brackets and may contain spaces; the
        // numbered fields start after the closing one.
        let fields: Vec<&str> = stat
            .rsplit_once(')')
            .map(|(_, rest)| rest.split_whitespace().collect())
            .unwrap_or_default();
        let number = |i: usize| fields.get(i).and_then(|v| v.parse::<u64>().ok());
        // Fields 14 and 15 overall (user and system time, in 1/100 s), which
        // are 11 and 12 here; 20 (threads) is 17; 24 (pages in memory) is 21.
        let ticks = number(11).zip(number(12)).map(|(user, system)| user + system);
        return Usage {
            memory_bytes: number(21).map(|pages| pages * 4096),
            cpu_time: ticks.map(|t| Duration::from_millis(t * 10)),
            threads: number(17),
        };
    }
    let memory = std::process::Command::new("ps")
        .args(["-o", "rss=", "-p", &std::process::id().to_string()])
        .output()
        .ok()
        .and_then(|out| String::from_utf8_lossy(&out.stdout).trim().parse::<u64>().ok())
        .map(|kilobytes| kilobytes * 1024);
    Usage {
        memory_bytes: memory,
        ..Usage::default()
    }
}

/// Total size of the files under a folder.
pub fn folder_size(path: &std::path::Path) -> u64 {
    let Ok(entries) = std::fs::read_dir(path) else {
        return 0;
    };
    entries
        .flatten()
        .map(|entry| match entry.metadata() {
            Ok(meta) if meta.is_dir() => folder_size(&entry.path()),
            Ok(meta) => meta.len(),
            Err(_) => 0,
        })
        .sum()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn latency_ignores_downloads_and_failures() {
        let shared: Shared = Shared::default();
        let ago = |ms: u64| Instant::now() - Duration::from_millis(ms);
        record(&shared, kind("/message/query"), ago(20), true);
        record(&shared, kind("/attachment/x/download"), ago(4000), true);
        record(&shared, kind("/chat/query"), ago(60), true);
        record(&shared, kind("/message/text"), ago(900), false);
        let m = shared.lock().unwrap();
        assert_eq!((m.requests, m.failures), (4, 1));
        let latency = m.latency();
        assert_eq!(latency.recent.len(), 2);
        assert_eq!(latency.slowest.unwrap().1, "chat list");
        assert_eq!(latency.last.unwrap().1, "chat list");
    }

    #[test]
    fn reads_its_own_usage() {
        let now = usage();
        // Memory is available on every platform we run on.
        assert!(now.memory_bytes.is_some_and(|b| b > 100_000));
    }
}
