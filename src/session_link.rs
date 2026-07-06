//! Attribute sessions to the account that was active when they ran, by joining
//! the switch `timeline` with session start times. Attribution is best-effort:
//! a session with no prior switch event is `unattributed` (a first-class
//! bucket), and a missing/older sessionwiki degrades gracefully (A14).

use crate::paths::Paths;
use serde_json::Value;
use std::collections::BTreeMap;

pub const UNATTRIBUTED: &str = "(unattributed)";

pub struct Event {
    pub ts: i64,
    pub tool: String,
    pub account: String,
}

pub fn read_timeline(paths: &Paths) -> Vec<Event> {
    let path = paths.store_dir().join("timeline.jsonl");
    let mut out = Vec::new();
    if let Ok(text) = std::fs::read_to_string(path) {
        for line in text.lines() {
            if let Ok(v) = serde_json::from_str::<Value>(line) {
                if let (Some(ts), Some(tool), Some(account)) =
                    (v["ts"].as_i64(), v["tool"].as_str(), v["account"].as_str())
                {
                    out.push(Event {
                        ts,
                        tool: tool.to_string(),
                        account: account.to_string(),
                    });
                }
            }
        }
    }
    out
}

/// The account active when a session of `tool` started: the last switch event
/// for that tool with `ts <= started`. None (unattributed) if none precedes it.
pub fn attribute(events: &[Event], tool: &str, started_secs: i64) -> Option<String> {
    events
        .iter()
        .filter(|e| e.tool == tool && e.ts <= started_secs)
        .max_by_key(|e| e.ts)
        .map(|e| e.account.clone())
}

/// Session counts per account, best-effort from `sessionwiki list --json`. None
/// if sessionwiki is absent/unusable - the caller degrades to "unavailable".
pub fn sessions_by_account(paths: &Paths) -> Option<BTreeMap<String, usize>> {
    let rows = sessionwiki_rows()?;
    let events = read_timeline(paths);
    let mut counts: BTreeMap<String, usize> = BTreeMap::new();
    for row in rows {
        let tool = match row["tool"].as_str() {
            Some(t) => t,
            None => continue,
        };
        let started = row["started"]
            .as_str()
            .and_then(rfc3339_to_secs)
            .unwrap_or(0);
        let acct = attribute(&events, tool, started).unwrap_or_else(|| UNATTRIBUTED.to_string());
        *counts.entry(acct).or_insert(0) += 1;
    }
    Some(counts)
}

pub fn status_line(paths: &Paths) -> Option<String> {
    // Until at least one switch is recorded, every session is unattributed and
    // the count is just the fetch cap - a confusing "N across 0 account(s)".
    // Say nothing rather than mislead.
    if read_timeline(paths).is_empty() {
        return None;
    }
    let counts = sessions_by_account(paths)?;
    let total: usize = counts.values().sum();
    let unattributed = counts.get(UNATTRIBUTED).copied().unwrap_or(0);
    let accounts = counts.keys().filter(|k| *k != UNATTRIBUTED).count();
    let tail = if unattributed > 0 {
        format!(", {unattributed} unattributed")
    } else {
        String::new()
    };
    Some(format!(
        "sessions: {total} across {accounts} account(s){tail} (sessionwiki)"
    ))
}

/// Run `sessionwiki list --json --no-sync` bounded by a short timeout, parsing
/// defensively. Any failure (absent binary, non-zero exit, unparseable, slow)
/// returns None so `status`/`sessions` never hangs or errors.
fn sessionwiki_rows() -> Option<Vec<Value>> {
    use std::process::{Command, Stdio};
    use std::sync::mpsc;
    // Under a dev/test root, sessionwiki would still read the HOST's real
    // sessions (it has no notion of SWAPDEX_ROOT), leaking them into an isolated
    // run. Skip it entirely in that mode.
    if std::env::var_os("SWAPDEX_ROOT").is_some() {
        return None;
    }
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let out = Command::new("sessionwiki")
            .args(["list", "--json", "--no-sync", "-n", "1000"])
            .stdin(Stdio::null())
            .output();
        let _ = tx.send(out);
    });
    let out = rx
        .recv_timeout(std::time::Duration::from_secs(2))
        .ok()?
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let v: Value = serde_json::from_slice(&out.stdout).ok()?;
    v.as_array().cloned()
}

pub(crate) fn rfc3339_to_secs(s: &str) -> Option<i64> {
    // Minimal parse: "YYYY-MM-DDTHH:MM:SS...": compute epoch seconds. Avoid a
    // chrono dep; only the ordering vs timeline ts matters, so a coarse but
    // monotonic value is fine. Fall back on any deviation.
    let bytes = s.as_bytes();
    if s.len() < 19 || bytes.get(4) != Some(&b'-') {
        return None;
    }
    let g = |a: usize, b: usize| s.get(a..b)?.parse::<i64>().ok();
    let (y, mo, d) = (g(0, 4)?, g(5, 7)?, g(8, 10)?);
    let (h, mi, se) = (g(11, 13)?, g(14, 16)?, g(17, 19)?);
    // days since epoch via a civil-from-date algorithm (Howard Hinnant).
    let yy = if mo <= 2 { y - 1 } else { y };
    let era = yy.div_euclid(400);
    let yoe = yy - era * 400;
    let doy = (153 * (if mo > 2 { mo - 3 } else { mo + 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146097 + doe - 719468;
    let naive = days * 86400 + h * 3600 + mi * 60 + se;
    // Normalize a trailing +HH:MM / -HH:MM offset to true UTC (UTC = local -
    // offset). A trailing "Z" or nothing is already UTC. Skip past fractional
    // seconds first ("...:00.123+09:00") or the offset would be missed.
    let mut off = &s[19..];
    if let Some(rest) = off.strip_prefix('.') {
        let digits = rest.chars().take_while(|c| c.is_ascii_digit()).count();
        off = &rest[digits..];
    }
    let offset_secs = if let Some(rest) = off.strip_prefix('+').or_else(|| off.strip_prefix('-')) {
        let sign = if off.starts_with('-') { -1 } else { 1 };
        let oh: i64 = rest.get(0..2).and_then(|x| x.parse().ok()).unwrap_or(0);
        let om: i64 = rest.get(3..5).and_then(|x| x.parse().ok()).unwrap_or(0);
        sign * (oh * 3600 + om * 60)
    } else {
        0
    };
    Some(naive - offset_secs)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ev(ts: i64, tool: &str, acct: &str) -> Event {
        Event {
            ts,
            tool: tool.into(),
            account: acct.into(),
        }
    }

    #[test]
    fn attribute_picks_the_last_switch_before_the_session() {
        let events = vec![
            ev(100, "codex", "work"),
            ev(200, "codex", "home"),
            ev(150, "claude-code", "personal"),
        ];
        // started at 250 -> last codex switch was 200 (home)
        assert_eq!(attribute(&events, "codex", 250).as_deref(), Some("home"));
        // started at 150 -> codex switch at 100 (work), not the 200 (later)
        assert_eq!(attribute(&events, "codex", 150).as_deref(), Some("work"));
        // started before ANY codex switch -> unattributed
        assert_eq!(attribute(&events, "codex", 50), None);
        // different tool timeline is independent
        assert_eq!(
            attribute(&events, "claude-code", 160).as_deref(),
            Some("personal")
        );
    }

    #[test]
    fn rfc3339_orders_correctly() {
        let a = rfc3339_to_secs("2026-06-10T10:00:00+00:00").unwrap();
        let b = rfc3339_to_secs("2026-06-10T10:00:01+00:00").unwrap();
        assert_eq!(b - a, 1);
        assert!(rfc3339_to_secs("2027-01-01T00:00:00Z").unwrap() > a);
    }

    #[test]
    fn rfc3339_offset_applies_after_fractional_seconds() {
        // A +09:00 offset behind fractional seconds must still normalize to
        // UTC (it used to be silently ignored).
        let utc = rfc3339_to_secs("2026-06-10T01:00:00Z").unwrap();
        let kst = rfc3339_to_secs("2026-06-10T10:00:00.123+09:00").unwrap();
        assert_eq!(kst, utc);
    }
}
