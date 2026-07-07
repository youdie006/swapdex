//! Local, no-network usage read. Sums tokens from each tool's on-disk session
//! transcripts over recent windows (5h / 7d) so you can tell how heavily you
//! have been using an account and when to switch. Reads `~/.claude/projects`
//! and `~/.codex/sessions` only - never the network.
//!
//! This is a rough local estimate of activity, not the vendor's billed quota:
//! transcripts are not tagged by account, so it reflects this machine's usage
//! for each tool, not a per-account remaining balance.
//!
//! Correctness notes (both verified against real transcripts):
//! - Claude writes one line per assistant CONTENT BLOCK, repeating the same
//!   `message.id` with identical usage - and a resumed session copies earlier
//!   messages into the new file. Dedupe by message id across all files or the
//!   totals overcount ~2.5x.
//! - Codex `last_token_usage` is per-request; `total_token_usage` is the
//!   monotonic running sum. Window the per-event DELTAS of the running sum by
//!   each line's timestamp.

use crate::paths::Paths;
use serde_json::Value;
use std::collections::HashSet;
use std::io::BufRead;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

const H5: u64 = 5 * 3600;
const D7: u64 = 7 * 86400;

#[derive(Default, Clone, Copy)]
pub struct Bucket {
    pub sessions: u64,
    pub tokens: u64,
}

pub struct ToolUsage {
    pub tool: &'static str,
    pub w5h: Bucket,
    pub w7d: Bucket,
    /// Tokens per account profile (from the switch timeline): name -> (5h, 7d).
    /// Empty when no switch history exists - attribution starts at the first
    /// switch, and swapdex never guesses.
    pub accounts: std::collections::BTreeMap<String, (u64, u64)>,
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn mtime_secs(p: &Path) -> u64 {
    std::fs::metadata(p)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Collect `*.jsonl` files under `dir` modified within `max_age` seconds (the
/// mtime gate keeps this fast even over thousands of transcripts).
fn recent_jsonl(dir: &Path, now: u64, max_age: u64, out: &mut Vec<PathBuf>) {
    if let Ok(rd) = std::fs::read_dir(dir) {
        for e in rd.flatten() {
            let p = e.path();
            if p.is_dir() {
                recent_jsonl(&p, now, max_age, out);
            } else if p.extension().is_some_and(|x| x == "jsonl")
                && now.saturating_sub(mtime_secs(&p)) <= max_age
            {
                out.push(p);
            }
        }
    }
}

pub fn tool_usage(paths: &Paths) -> Vec<ToolUsage> {
    let now = now_secs();
    // The switch timeline turns machine-wide token counts into per-ACCOUNT
    // ones: each event is attributed to the profile active at its timestamp
    // (the same join `sessions` uses).
    let events = crate::session_link::read_timeline(paths);
    // Per-file parsed-events cache: a heavy week holds ~1GB of transcripts,
    // and only files whose (mtime,size) changed need reparsing.
    let cache_path = paths.store_dir().join("usage-cache.json");
    let mut cache: UsageCache = std::fs::read(&cache_path)
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_default();
    let before = cache.len();
    let (c5, c7, ca) = claude_usage(&paths.claude_projects(), now, &events, &mut cache);
    let (x5, x7, xa) = codex_usage(&paths.codex_sessions(), now, &events, &mut cache);
    // Prune entries that fell out of the 7d window or were deleted, then
    // persist (atomic, 0600) - best-effort: usage must work without a store.
    let live: std::collections::HashSet<String> = {
        let mut f = Vec::new();
        recent_jsonl(&paths.claude_projects(), now, D7, &mut f);
        recent_jsonl(&paths.codex_sessions(), now, D7, &mut f);
        f.iter().map(|p| p.to_string_lossy().into_owned()).collect()
    };
    let had_misses = cache.len() != before || cache.keys().any(|k| !live.contains(k));
    cache.retain(|k, _| live.contains(k));
    if had_misses && std::fs::create_dir_all(paths.store_dir()).is_ok() {
        if let Ok(bytes) = serde_json::to_vec(&cache) {
            let _ = crate::atomic::write_secret(&cache_path, &bytes);
        }
    }
    vec![
        ToolUsage {
            tool: "claude-code",
            w5h: c5,
            w7d: c7,
            accounts: ca,
        },
        ToolUsage {
            tool: "codex",
            w5h: x5,
            w7d: x7,
            accounts: xa,
        },
    ]
}

/// Attribute `toks` at `ts` to the account active then (per-tool), adding into
/// the 5h/7d columns of `map`. No events for the tool before `ts` -> no entry
/// (never a guess).
fn credit(
    map: &mut std::collections::BTreeMap<String, (u64, u64)>,
    events: &[crate::session_link::Event],
    tool: &str,
    ts: u64,
    now: u64,
    toks: u64,
) {
    if toks == 0 {
        return;
    }
    let Some(account) = crate::session_link::attribute(events, tool, ts as i64) else {
        return;
    };
    let e = map.entry(account).or_insert((0, 0));
    let age = now.saturating_sub(ts);
    if age <= D7 {
        e.1 += toks;
    }
    if age <= H5 {
        e.0 += toks;
    }
}

/// The line's timestamp in unix seconds, or None.
fn line_ts(d: &Value) -> Option<u64> {
    let t = d["timestamp"]
        .as_str()
        .and_then(crate::session_link::rfc3339_to_secs)?;
    (t >= 0).then_some(t as u64)
}

/// One transcript file's parsed usage events - the cacheable unit. `id` is
/// the claude message id ("" when absent / codex); `toks` is claude's total
/// or codex's per-event DELTA (per-file local state, so safe to cache).
#[derive(serde::Serialize, serde::Deserialize, Clone, Default)]
pub struct FileEvents {
    pub mtime: u64,
    pub size: u64,
    pub ev: Vec<(String, u64, u64)>, // (id, ts, toks)
}

pub type UsageCache = std::collections::HashMap<String, FileEvents>;

fn file_sig(p: &Path) -> (u64, u64) {
    let m = std::fs::metadata(p).ok();
    (
        m.as_ref()
            .and_then(|m| m.modified().ok())
            .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
            .map(|d| d.as_secs())
            .unwrap_or(0),
        m.map(|m| m.len()).unwrap_or(0),
    )
}

/// Fetch each file's events, from the cache when (mtime,size) match, parsing
/// (in parallel) otherwise. The 7-day window holds ~1GB of transcripts on a
/// heavy machine - reparsing all of it on every `usage` took ~20s cold.
fn events_for_files(
    files: &[PathBuf],
    cache: &mut UsageCache,
    parse: fn(&Path) -> Vec<(String, u64, u64)>,
) -> Vec<(String, FileEvents)> {
    let mut out = Vec::with_capacity(files.len());
    let mut misses: Vec<(String, u64, u64, &PathBuf)> = Vec::new();
    for f in files {
        let key = f.to_string_lossy().into_owned();
        let (mtime, size) = file_sig(f);
        match cache.get(&key) {
            Some(c) if c.mtime == mtime && c.size == size => {}
            _ => misses.push((key, mtime, size, f)),
        }
    }
    let threads = misses.len().clamp(1, 8);
    let chunks: Vec<Vec<(String, u64, u64, PathBuf)>> = {
        let mut cs: Vec<Vec<(String, u64, u64, PathBuf)>> =
            (0..threads).map(|_| Vec::new()).collect();
        for (i, (k, m, sz, f)) in misses.into_iter().enumerate() {
            cs[i % threads].push((k, m, sz, f.clone()));
        }
        cs
    };
    let parsed: Vec<(String, FileEvents)> = std::thread::scope(|scope| {
        let handles: Vec<_> = chunks
            .into_iter()
            .map(|chunk| {
                scope.spawn(move || {
                    chunk
                        .into_iter()
                        .map(|(k, mtime, size, f)| {
                            (
                                k,
                                FileEvents {
                                    mtime,
                                    size,
                                    ev: parse(&f),
                                },
                            )
                        })
                        .collect::<Vec<_>>()
                })
            })
            .collect();
        handles
            .into_iter()
            .flat_map(|h| h.join().unwrap_or_default())
            .collect()
    });
    for (k, fe) in parsed {
        cache.insert(k, fe);
    }
    // Emit in the CALLER'S file order, not hits-then-misses: claude's global
    // message-id dedupe is first-wins, so ordering decides which FILE's
    // session buckets get a duplicated message - totals must not depend on
    // what happened to be cached.
    out.clear();
    for f in files {
        let key = f.to_string_lossy().into_owned();
        if let Some(fe) = cache.get(&key) {
            out.push((key, fe.clone()));
        }
    }
    out
}

fn parse_claude_file(f: &Path) -> Vec<(String, u64, u64)> {
    let Ok(file) = std::fs::File::open(f) else {
        return Vec::new();
    };
    let mut ev = Vec::new();
    for line in std::io::BufReader::new(file).lines() {
        let Ok(line) = line else { break };
        if !line.contains("\"usage\"") {
            continue;
        }
        let d: Value = match serde_json::from_str(&line) {
            Ok(v) => v,
            Err(_) => continue,
        };
        let u = &d["message"]["usage"];
        if !u.is_object() {
            continue;
        }
        let Some(ts) = line_ts(&d) else { continue };
        let toks = u["input_tokens"].as_u64().unwrap_or(0)
            + u["output_tokens"].as_u64().unwrap_or(0)
            + u["cache_creation_input_tokens"].as_u64().unwrap_or(0);
        let id = d["message"]["id"].as_str().unwrap_or("").to_string();
        ev.push((id, ts, toks));
    }
    ev
}

fn parse_codex_file(f: &Path) -> Vec<(String, u64, u64)> {
    let Ok(file) = std::fs::File::open(f) else {
        return Vec::new();
    };
    let mut ev = Vec::new();
    let mut prev = 0u64;
    for line in std::io::BufReader::new(file).lines() {
        let Ok(line) = line else { break };
        if !line.contains("total_token_usage") {
            continue;
        }
        let d: Value = match serde_json::from_str(&line) {
            Ok(v) => v,
            Err(_) => continue,
        };
        let tot = &d["payload"]["info"]["total_token_usage"];
        if !tot.is_object() {
            continue;
        }
        let cur = match tot["total_tokens"].as_u64() {
            Some(t) => t,
            None => {
                tot["input_tokens"].as_u64().unwrap_or(0)
                    + tot["output_tokens"].as_u64().unwrap_or(0)
            }
        };
        // Duplicate token_count lines repeat the same running sum; the
        // saturating delta makes them contribute zero.
        let delta = cur.saturating_sub(prev);
        prev = prev.max(cur);
        if delta == 0 {
            continue;
        }
        let ts = line_ts(&d).unwrap_or_else(|| mtime_secs(f));
        ev.push((String::new(), ts, delta));
    }
    ev
}

/// Claude: one API call = one `message.id`; a call's usage may be repeated on
/// several lines (one per content block) and whole messages reappear in resumed
/// sessions, so count each id once globally. Window by the message timestamp.
fn claude_usage(
    dir: &Path,
    now: u64,
    events: &[crate::session_link::Event],
    cache: &mut UsageCache,
) -> (
    Bucket,
    Bucket,
    std::collections::BTreeMap<String, (u64, u64)>,
) {
    let mut files = Vec::new();
    recent_jsonl(dir, now, D7, &mut files);
    let per_file = events_for_files(&files, cache, parse_claude_file);
    let mut seen: HashSet<String> = HashSet::new();
    let mut accounts = std::collections::BTreeMap::new();
    let (mut b5, mut b7) = (Bucket::default(), Bucket::default());
    for (_key, fe) in &per_file {
        let (mut t5, mut t7, mut in5, mut in7) = (0u64, 0u64, false, false);
        for (id, ts, toks) in &fe.ev {
            if !id.is_empty() && !seen.insert(id.clone()) {
                continue; // repeated content-block line or resumed-session copy
            }
            let age = now.saturating_sub(*ts);
            if age <= D7 {
                t7 += toks;
                in7 = true;
            }
            if age <= H5 {
                t5 += toks;
                in5 = true;
            }
            credit(&mut accounts, events, "claude-code", *ts, now, *toks);
        }
        b7.tokens += t7;
        b7.sessions += in7 as u64;
        b5.tokens += t5;
        b5.sessions += in5 as u64;
    }
    (b5, b7, accounts)
}

/// Codex: `payload.info.total_token_usage` is the monotonic running sum for the
/// session; the tokens attributable to one event are the DELTA from the
/// previous event, windowed by that line's timestamp.
fn codex_usage(
    dir: &Path,
    now: u64,
    events: &[crate::session_link::Event],
    cache: &mut UsageCache,
) -> (
    Bucket,
    Bucket,
    std::collections::BTreeMap<String, (u64, u64)>,
) {
    let mut files = Vec::new();
    recent_jsonl(dir, now, D7, &mut files);
    let per_file = events_for_files(&files, cache, parse_codex_file);
    let mut accounts = std::collections::BTreeMap::new();
    let (mut b5, mut b7) = (Bucket::default(), Bucket::default());
    for (_key, fe) in &per_file {
        let (mut t5, mut t7, mut in5, mut in7) = (0u64, 0u64, false, false);
        for (_id, ts, delta) in &fe.ev {
            let age = now.saturating_sub(*ts);
            if age <= D7 {
                t7 += delta;
                in7 = true;
            }
            if age <= H5 {
                t5 += delta;
                in5 = true;
            }
            credit(&mut accounts, events, "codex", *ts, now, *delta);
        }
        b7.tokens += t7;
        b7.sessions += in7 as u64;
        b5.tokens += t5;
        b5.sessions += in5 as u64;
    }
    (b5, b7, accounts)
}

/// Human-friendly token count (e.g. 1.2M, 45.0k, 900). Thresholds are set just
/// below each unit so a value that would ROUND to 1000.0k prints as 1.0M.
pub fn human(n: u64) -> String {
    if n >= 999_950_000 {
        format!("{:.1}B", n as f64 / 1_000_000_000.0)
    } else if n >= 999_950 {
        format!("{:.1}M", n as f64 / 1_000_000.0)
    } else if n >= 1_000 {
        format!("{:.1}k", n as f64 / 1_000.0)
    } else {
        n.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn human_formats_counts() {
        assert_eq!(human(900), "900");
        assert_eq!(human(45_000), "45.0k");
        assert_eq!(human(1_200_000), "1.2M");
        assert_eq!(human(999_999), "1.0M", "never prints 1000.0k");
        assert_eq!(human(999_999_999), "1.0B", "never prints 1000.0M");
    }

    #[test]
    fn claude_dedupes_repeated_message_ids() {
        // Fixed `now` + fixed timestamps -> deterministic windowing.
        let now = crate::session_link::rfc3339_to_secs("2026-07-06T12:00:00Z").unwrap() as u64;
        let dir = tempfile::tempdir().unwrap();
        let proj = dir.path().join("p");
        std::fs::create_dir_all(&proj).unwrap();
        // The same message id appears on two lines (two content blocks) with
        // identical usage, plus one old message outside every window.
        let m1 = serde_json::json!({"timestamp": "2026-07-06T11:59:00Z",
            "message": {"id": "msg_1", "usage": {"input_tokens": 100, "output_tokens": 50}}});
        let old = serde_json::json!({"timestamp": "2026-06-01T00:00:00Z",
            "message": {"id": "msg_0", "usage": {"input_tokens": 999, "output_tokens": 999}}});
        std::fs::write(proj.join("s.jsonl"), format!("{m1}\n{m1}\n{old}\n")).unwrap();
        // A resumed session copies msg_1 into a second file: still counted once.
        std::fs::write(proj.join("resumed.jsonl"), format!("{m1}\n")).unwrap();
        let (b5, b7, _a) = claude_usage(dir.path(), now, &[], &mut UsageCache::default());
        assert_eq!(
            b5.tokens, 150,
            "duplicate lines and resumed copies count once"
        );
        assert_eq!(b7.tokens, 150, "the 7d-old message is excluded");
    }

    #[test]
    fn cache_hits_give_identical_totals_and_invalidate_on_change() {
        let now = crate::session_link::rfc3339_to_secs("2026-07-06T12:00:00Z").unwrap() as u64;
        let dir = tempfile::tempdir().unwrap();
        let proj = dir.path().join("p");
        std::fs::create_dir_all(&proj).unwrap();
        let m1 = serde_json::json!({"timestamp": "2026-07-06T11:59:00Z",
            "message": {"id": "msg_1", "usage": {"input_tokens": 100, "output_tokens": 50}}});
        std::fs::write(proj.join("s.jsonl"), format!("{m1}\n")).unwrap();
        let mut cache = UsageCache::default();
        let (_b5, b7, _a) = claude_usage(dir.path(), now, &[], &mut cache);
        assert_eq!(b7.tokens, 150);
        assert_eq!(cache.len(), 1, "file cached");
        // Second run: cache hit, identical totals (the parse fn is not
        // consulted for unchanged files - prove by corrupting the FILE but
        // keeping mtime+size... simpler: same totals from the same cache).
        let (_b5, b7b, _a) = claude_usage(dir.path(), now, &[], &mut cache);
        assert_eq!(b7b.tokens, 150, "cached run identical");
        // Change the file (size changes) -> reparse picks up the new event.
        let m2 = serde_json::json!({"timestamp": "2026-07-06T11:58:00Z",
            "message": {"id": "msg_2", "usage": {"input_tokens": 10, "output_tokens": 0}}});
        std::fs::write(proj.join("s.jsonl"), format!("{m1}\n{m2}\n")).unwrap();
        let (_b5, b7c, _a) = claude_usage(dir.path(), now, &[], &mut cache);
        assert_eq!(b7c.tokens, 160, "size change invalidates the entry");
    }

    #[test]
    fn codex_uses_cumulative_total_deltas() {
        let now = crate::session_link::rfc3339_to_secs("2026-07-06T12:00:00Z").unwrap() as u64;
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("2026")).unwrap();
        // Running sum 1500 -> 1800 (with a duplicated line): total is 1800,
        // NOT the last request's 300 and NOT 1500+1800+1800.
        let l1 = serde_json::json!({"timestamp": "2026-07-06T11:00:00Z", "type": "event_msg",
            "payload": {"info": {"last_token_usage": {"input_tokens": 1400, "output_tokens": 100},
                                  "total_token_usage": {"input_tokens": 1400, "output_tokens": 100, "total_tokens": 1500}}}});
        let l2 = serde_json::json!({"timestamp": "2026-07-06T11:30:00Z", "type": "event_msg",
            "payload": {"info": {"last_token_usage": {"input_tokens": 250, "output_tokens": 50},
                                  "total_token_usage": {"input_tokens": 1650, "output_tokens": 150, "total_tokens": 1800}}}});
        std::fs::write(
            dir.path().join("2026").join("rollout-x.jsonl"),
            format!("{l1}\n{l2}\n{l2}\n"),
        )
        .unwrap();
        let (b5, b7, _a) = codex_usage(dir.path(), now, &[], &mut UsageCache::default());
        assert_eq!(b5.tokens, 1800, "cumulative total, duplicates ignored");
        assert_eq!(b7.tokens, 1800);
        assert_eq!(b5.sessions, 1);
    }

    #[test]
    fn tokens_attribute_to_the_account_active_at_event_time() {
        let now = crate::session_link::rfc3339_to_secs("2026-07-06T12:00:00Z").unwrap() as u64;
        let dir = tempfile::tempdir().unwrap();
        let proj = dir.path().join("p");
        std::fs::create_dir_all(&proj).unwrap();
        // work until 10:00, personal after.
        let t10 = crate::session_link::rfc3339_to_secs("2026-07-06T10:00:00Z").unwrap();
        let mk = |ts: i64, account: &str| crate::session_link::Event {
            ts,
            tool: "claude-code".into(),
            account: account.into(),
        };
        let events = vec![mk(0, "work"), mk(t10, "personal")];
        let m = |ts: &str, tok: u64| {
            serde_json::json!({"timestamp": ts,
                "message": {"id": format!("m{ts}{tok}"), "usage": {"input_tokens": tok, "output_tokens": 0}}})
        };
        std::fs::write(
            proj.join("s.jsonl"),
            format!(
                "{}\n{}\n",
                m("2026-07-06T06:00:00Z", 100), // -> work (6h ago: outside 5h)
                m("2026-07-06T11:00:00Z", 40),  // -> personal
            ),
        )
        .unwrap();
        let (_b5, b7, accounts) =
            claude_usage(dir.path(), now, &events, &mut UsageCache::default());
        assert_eq!(b7.tokens, 140);
        assert_eq!(
            accounts.get("work"),
            Some(&(0, 100)),
            "6h ago is outside 5h"
        );
        assert_eq!(accounts.get("personal"), Some(&(40, 40)));
        // No events -> no attribution at all (never a guess).
        let (_b5, _b7, none) = claude_usage(dir.path(), now, &[], &mut UsageCache::default());
        assert!(none.is_empty());
    }

    #[test]
    fn codex_windows_deltas_by_event_time_not_mtime() {
        let now = crate::session_link::rfc3339_to_secs("2026-07-06T12:00:00Z").unwrap() as u64;
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("d")).unwrap();
        // A long-running session: 1000 tokens 2 days ago, 500 within the 5h
        // window. The 5h bucket must see only the recent 500.
        let l1 = serde_json::json!({"timestamp": "2026-07-04T12:00:00Z",
            "payload": {"info": {"total_token_usage": {"total_tokens": 1000}}}});
        let l2 = serde_json::json!({"timestamp": "2026-07-06T11:00:00Z",
            "payload": {"info": {"total_token_usage": {"total_tokens": 1500}}}});
        std::fs::write(
            dir.path().join("d").join("rollout-y.jsonl"),
            format!("{l1}\n{l2}\n"),
        )
        .unwrap();
        let (b5, b7, _a) = codex_usage(dir.path(), now, &[], &mut UsageCache::default());
        assert_eq!(b5.tokens, 500, "only the in-window delta");
        assert_eq!(b7.tokens, 1500);
    }
}
