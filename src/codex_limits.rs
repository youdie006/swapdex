//! Codex's own rate limits, read from its session logs - no network at all.
//!
//! The Codex CLI records what the API told it about the account's windows into
//! the session transcript: `payload.rate_limits.primary` / `.secondary`, each
//! carrying `used_percent`, `window_minutes` and `resets_at`. So a Codex
//! account's usage can be shown the same way Claude's is, except it costs a local
//! file read instead of an HTTP request.
//!
//! The transcript does NOT say which account it belongs to, so what this yields
//! is "the newest limits Codex saw", which belong to the account signed in at
//! that moment - the active one. It is therefore reported for the active Codex
//! account only, rather than guessed at for the others.

use crate::paths::Paths;
use std::path::{Path, PathBuf};

/// One window as Codex reports it.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Window {
    pub used_pct: f64,
    /// The window's length in minutes: 300 is the 5-hour window, 10080 a week.
    pub window_minutes: i64,
    pub resets_at: Option<i64>,
}

/// Both of an account's windows, shortest first (so `.0` is the session window
/// and `.1` the longer one, whatever lengths the API happens to use).
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Limits {
    pub short: Option<Window>,
    pub long: Option<Window>,
}

fn window_from(v: &serde_json::Value) -> Option<Window> {
    let used_pct = v.get("used_percent")?.as_f64()?;
    Some(Window {
        used_pct,
        window_minutes: v
            .get("window_minutes")
            .and_then(serde_json::Value::as_i64)
            .unwrap_or(0),
        resets_at: v.get("resets_at").and_then(serde_json::Value::as_i64),
    })
}

/// Pull the newest `rate_limits` block and the session's `email` out of one
/// transcript. Scans from the end: the last block is the current picture.
fn from_transcript(path: &Path) -> Option<Limits> {
    let text = std::fs::read_to_string(path).ok()?;
    let mut limits: Option<Limits> = None;
    for line in text.lines() {
        // Cheap prefilter: parsing every line of a long transcript is the slow
        // part, and only a few carry this field.
        if !line.contains("\"rate_limits\"") {
            continue;
        }
        let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        {
            if let Some(rl) = find_key(&v, "rate_limits") {
                let short_first = |a: Option<Window>, b: Option<Window>| match (a, b) {
                    (Some(x), Some(y)) if y.window_minutes < x.window_minutes => (Some(y), Some(x)),
                    (a, b) => (a, b),
                };
                let (short, long) = short_first(
                    rl.get("primary").and_then(window_from),
                    rl.get("secondary").and_then(window_from),
                );
                limits = Some(Limits { short, long });
            }
        }
    }
    limits
}

/// Depth-first search for the first value under `key` anywhere in the object -
/// the transcript nests these differently across event types.
fn find_key<'a>(v: &'a serde_json::Value, key: &str) -> Option<&'a serde_json::Value> {
    match v {
        serde_json::Value::Object(m) => {
            if let Some(found) = m.get(key) {
                if !found.is_null() {
                    return Some(found);
                }
            }
            m.values().find_map(|x| find_key(x, key))
        }
        serde_json::Value::Array(a) => a.iter().find_map(|x| find_key(x, key)),
        _ => None,
    }
}

/// The newest limits Codex recorded, from transcripts touched within
/// `max_age_secs`. `None` when no recent transcript carries any - Codex only
/// writes them once the API has reported a window.
pub fn latest(paths: &Paths, now: u64, max_age_secs: u64) -> Option<Limits> {
    let mut files: Vec<PathBuf> = Vec::new();
    collect_jsonl(&paths.codex_sessions(), now, max_age_secs, &mut files);
    // Newest first, and stop at the first transcript that actually has limits.
    files.sort_by_key(|p| std::cmp::Reverse(mtime_secs(p)));
    files.iter().find_map(|f| from_transcript(f))
}

fn mtime_secs(p: &Path) -> u64 {
    std::fs::metadata(p)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// `*.jsonl` under `dir` modified within `max_age` seconds. The mtime gate is
/// what keeps this fast across thousands of transcripts.
fn collect_jsonl(dir: &Path, now: u64, max_age: u64, out: &mut Vec<PathBuf>) {
    if let Ok(rd) = std::fs::read_dir(dir) {
        for e in rd.flatten() {
            let p = e.path();
            if p.is_dir() {
                collect_jsonl(&p, now, max_age, out);
            } else if p.extension().is_some_and(|x| x == "jsonl")
                && now.saturating_sub(mtime_secs(&p)) <= max_age
            {
                out.push(p);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The real transcript shape: `payload.rate_limits`, no account identity.
    fn write_transcript(dir: &Path, name: &str, primary: f64, secondary: Option<f64>) {
        let sec = match secondary {
            Some(p) => {
                format!(r#"{{"used_percent":{p},"window_minutes":300,"resets_at":1785600000}}"#)
            }
            None => "null".into(),
        };
        let body = format!(
            "{{\"payload\":{{\"type\":\"other\"}}}}\n{{\"payload\":{{\"rate_limits\":{{\"primary\":{{\"used_percent\":{primary},\"window_minutes\":10080,\"resets_at\":1785611966}},\"secondary\":{sec}}}}}}}\n"
        );
        std::fs::write(dir.join(name), body).unwrap();
    }

    #[test]
    fn reads_the_windows_and_orders_them_shortest_first() {
        let d = tempfile::tempdir().unwrap();
        write_transcript(d.path(), "a.jsonl", 16.0, Some(42.0));
        let limits = from_transcript(&d.path().join("a.jsonl")).expect("parsed");
        // The 300-minute window is the session one, so it sorts first even though
        // the API called it "secondary".
        assert_eq!(limits.short.unwrap().used_pct, 42.0);
        assert_eq!(limits.short.unwrap().window_minutes, 300);
        assert_eq!(limits.long.unwrap().used_pct, 16.0);
        assert_eq!(limits.long.unwrap().window_minutes, 10080);
        assert_eq!(limits.long.unwrap().resets_at, Some(1785611966));
    }

    #[test]
    fn a_single_window_is_reported_alone() {
        let d = tempfile::tempdir().unwrap();
        write_transcript(d.path(), "b.jsonl", 7.5, None);
        let limits = from_transcript(&d.path().join("b.jsonl")).expect("parsed");
        assert_eq!(
            limits.short.unwrap().used_pct,
            7.5,
            "the only window is first"
        );
        assert!(
            limits.long.is_none(),
            "nothing invented for the missing one"
        );
    }

    #[test]
    fn a_transcript_without_limits_yields_nothing() {
        let d = tempfile::tempdir().unwrap();
        std::fs::write(d.path().join("c.jsonl"), b"{\"payload\":{\"x\":1}}\n").unwrap();
        assert!(from_transcript(&d.path().join("c.jsonl")).is_none());
        // Corrupt lines are skipped rather than failing the read.
        std::fs::write(d.path().join("d.jsonl"), b"{ broken\n").unwrap();
        assert!(from_transcript(&d.path().join("d.jsonl")).is_none());
    }

    #[test]
    fn latest_prefers_the_newest_transcript_that_has_limits() {
        let d = tempfile::tempdir().unwrap();
        let sessions = d.path().join(".codex/sessions/2026/07/27");
        std::fs::create_dir_all(&sessions).unwrap();
        write_transcript(&sessions, "old.jsonl", 10.0, None);
        // A second of separation makes the mtime order unambiguous without
        // needing a crate to backdate a file.
        std::thread::sleep(std::time::Duration::from_millis(1100));
        write_transcript(&sessions, "new.jsonl", 55.0, None);
        let now = std::time::SystemTime::now();
        let paths = Paths::rooted(d.path());
        let secs = now.duration_since(std::time::UNIX_EPOCH).unwrap().as_secs();
        let got = latest(&paths, secs, 86400).expect("found");
        assert_eq!(got.short.unwrap().used_pct, 55.0, "the newest one wins");
        // A transcript older than the window is not consulted at all.
        assert!(latest(&paths, secs + 200_000, 3600).is_none());
    }
}
