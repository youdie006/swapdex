//! Codex's own rate limits, read from its session logs - no network at all.
//!
//! The Codex CLI records what the API told it about the account's windows into
//! the session transcript: `payload.rate_limits.primary` / `.secondary`, each
//! carrying `used_percent`, `window_minutes` and `resets_at`. So a Codex
//! account's usage can be shown the same way Claude's is, except it costs a local
//! file read instead of an HTTP request.
//!
//! The transcript does NOT say which account it belongs to - not in the
//! `rate_limits` block, not in the session header, nowhere in the file. So a
//! reading can only be attributed by WHERE it was read from, and each account's
//! home is read separately.
//!
//! That is not a compromise, it is the right answer. Measured on a real machine:
//! a Codex turn driven through swapdex's proxy carries no rate limits in its
//! response headers and none in its SSE body, and only two requests reach the
//! proxy - yet the transcript gains a `rate_limits` entry. The CLI fetches its
//! limits by a path the proxy never sees, using its OWN login. What lands in a
//! home therefore describes that home's account, whoever happened to be paying
//! for the turns.
//!
//! An earlier version captioned each reading with the payer from the switch
//! timeline. No other tool surveyed does that; they all bind a reading to the
//! credential that fetched it, which here is the home. It also produced the
//! visible symptom that started this: an account with no transcripts at all
//! showing a reading, beside one holding 458 of them showing none.
//!
//! Upstream will not close the gap - openai/codex#16323 asked for a user id
//! next to `rate_limits` and was declined, noting that on Team plans quotas are
//! per USER while the account id is shared, so even that would not have been
//! enough.

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
    /// Unix seconds when the API stated these windows, taken from the record
    /// that carried them.
    ///
    /// It used to be the transcript's mtime, which moves every time Codex writes
    /// anything at all. A conversation that kept running without the API
    /// restating the windows made an hours-old snapshot look freshly taken - and
    /// the age IS the caveat here, since there is no endpoint to ask.
    pub observed_at: Option<i64>,
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

/// Pull the newest `rate_limits` block out of one transcript.
///
/// The block carries `limit_id`, `plan_type` and the windows - and NO account
/// identifier. Neither does the session header. So a reading taken from here
/// cannot say whose it is; only where it was read from. This used to claim it
/// also pulled "the session's email", which the transcript has never contained,
/// and reading that comment is how someone would conclude these numbers arrive
/// already attributed.
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
                limits = Some(Limits {
                    short,
                    long,
                    // The record's own stamp, when it carries one.
                    observed_at: v
                        .get("timestamp")
                        .and_then(serde_json::Value::as_str)
                        .and_then(crate::session_link::rfc3339_to_secs),
                });
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
/// The same reading, for ONE account's home.
///
/// Only the bare `~/.codex` was ever read, so an account that is a slot - which
/// is what `run`, `adopt` and `onboard` create - had its transcripts sitting in
/// a directory nothing looked at, and got no usage at all while another home's
/// numbers were displayed beside it.
pub fn for_slot(config_dir: &Path, now: u64, max_age_secs: u64) -> Option<Limits> {
    from_sessions_dir(&config_dir.join("sessions"), now, max_age_secs)
}

fn from_sessions_dir(dir: &Path, now: u64, max_age_secs: u64) -> Option<Limits> {
    let mut files: Vec<PathBuf> = Vec::new();
    collect_jsonl(dir, now, max_age_secs, &mut files);
    // Newest first, and stop at the first transcript that actually has limits.
    files.sort_by_key(|p| std::cmp::Reverse(mtime_secs(p)));
    let (path, raw) = files
        .iter()
        .find_map(|f| from_transcript(f).map(|l| (f, l)))?;
    let still_valid = |w: Option<Window>| w.filter(|w| w.resets_at.is_none_or(|r| r > now as i64));
    let l = Limits {
        short: still_valid(raw.short),
        long: still_valid(raw.long),
        // The file's mtime only stands in when the record carried no stamp.
        observed_at: raw.observed_at.or(Some(mtime_secs(path) as i64)),
    };
    (l.short.is_some() || l.long.is_some()).then_some(l)
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
    use crate::paths::Paths;

    /// A slot with no transcripts of its own reports nothing.
    ///
    /// This is the shape the payer caption broke: on a real machine an account
    /// with zero session files showed a reading, because a reading taken from
    /// ANOTHER home was captioned with whoever the timeline said was paying.
    /// A reading belongs to the home it was read from, so a home with none has
    /// none.
    #[test]
    fn a_home_with_no_transcripts_reports_nothing() {
        let d = tempfile::tempdir().unwrap();
        let with = d.path().join("has/sessions/2026/08/14");
        let without = d.path().join("none");
        std::fs::create_dir_all(&with).unwrap();
        std::fs::create_dir_all(without.join("sessions")).unwrap();
        write_transcript(&with, "a.jsonl", 16.0, Some(42.0));

        // Real "now": the age gate compares against the files just written, and
        // a far-future now would filter them out before the reset check ever
        // mattered - which is exactly what the first version of this test did.
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        assert!(
            for_slot(&d.path().join("has"), now, 10 * 86400).is_some(),
            "the home that holds the transcript reports its reading"
        );
        assert!(
            for_slot(&without, now, 10 * 86400).is_none(),
            "a home with no transcripts reports nothing - never another home's numbers"
        );
    }

    /// Two fixed moments for the fixtures: a window that has NOT reset and one
    /// that has. They were literal timestamps once, which passed until the day
    /// they went by in the real world and the test failed for the calendar
    /// rather than for the code.
    const LIVE_RESET: i64 = 4_102_444_800; // 2100-01-01
    const PAST_RESET: i64 = 1_000_000_000; // 2001-09-09

    /// The real transcript shape: `payload.rate_limits`, no account identity.
    fn write_transcript(dir: &Path, name: &str, primary: f64, secondary: Option<f64>) {
        write_transcript_resetting(dir, name, primary, secondary, LIVE_RESET)
    }

    /// The same, with the reset moment chosen by the caller.
    fn write_transcript_resetting(
        dir: &Path,
        name: &str,
        primary: f64,
        secondary: Option<f64>,
        reset: i64,
    ) {
        let sec = match secondary {
            Some(p) => {
                format!(r#"{{"used_percent":{p},"window_minutes":300,"resets_at":{reset}}}"#)
            }
            None => "null".into(),
        };
        let body = format!(
            "{{\"payload\":{{\"type\":\"other\"}}}}\n{{\"payload\":{{\"rate_limits\":{{\"primary\":{{\"used_percent\":{primary},\"window_minutes\":10080,\"resets_at\":{reset}}},\"secondary\":{sec}}}}}}}\n"
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
        assert_eq!(limits.long.unwrap().resets_at, Some(LIVE_RESET));
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

    // A window whose reset has passed describes a window that no longer exists,
    // so it is dropped rather than reported as still-used.
    #[test]
    fn a_window_past_its_reset_is_not_reported() {
        let d = tempfile::tempdir().unwrap();
        let sessions = d.path().join(".codex/sessions/2026/07/27");
        std::fs::create_dir_all(&sessions).unwrap();
        write_transcript_resetting(&sessions, "a.jsonl", 16.0, Some(42.0), PAST_RESET);
        let paths = Paths::rooted(d.path());
        // "Now" before the reset: both windows stand.
        let l = for_slot(paths.codex_dir(), PAST_RESET as u64 - 1, 10 * 86400)
            .expect("both windows live");
        assert!(l.short.is_some() && l.long.is_some());
        // "Now" after it: nothing to report rather than numbers describing a
        // window that no longer exists.
        assert!(
            for_slot(paths.codex_dir(), PAST_RESET as u64 + 1, 10 * 86400).is_none(),
            "a reset window is not reported as used"
        );
    }

    #[test]
    fn the_newest_transcript_that_has_limits_wins() {
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
        let got = for_slot(paths.codex_dir(), secs, 86400).expect("found");
        assert_eq!(got.short.unwrap().used_pct, 55.0, "the newest one wins");
        // A transcript older than the window is not consulted at all.
        assert!(for_slot(paths.codex_dir(), secs + 200_000, 3600).is_none());
    }
}
