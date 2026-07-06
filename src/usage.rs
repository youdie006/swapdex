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
    let (c5, c7) = claude_usage(&paths.claude_projects(), now);
    let (x5, x7) = codex_usage(&paths.codex_sessions(), now);
    vec![
        ToolUsage {
            tool: "claude-code",
            w5h: c5,
            w7d: c7,
        },
        ToolUsage {
            tool: "codex",
            w5h: x5,
            w7d: x7,
        },
    ]
}

/// The line's timestamp in unix seconds, or None.
fn line_ts(d: &Value) -> Option<u64> {
    let t = d["timestamp"]
        .as_str()
        .and_then(crate::session_link::rfc3339_to_secs)?;
    (t >= 0).then_some(t as u64)
}

/// Claude: one API call = one `message.id`; a call's usage may be repeated on
/// several lines (one per content block) and whole messages reappear in resumed
/// sessions, so count each id once globally. Window by the message timestamp.
fn claude_usage(dir: &Path, now: u64) -> (Bucket, Bucket) {
    let mut files = Vec::new();
    recent_jsonl(dir, now, D7, &mut files);
    let mut seen: HashSet<String> = HashSet::new();
    let (mut b5, mut b7) = (Bucket::default(), Bucket::default());
    for f in &files {
        let Ok(file) = std::fs::File::open(f) else {
            continue;
        };
        let (mut t5, mut t7, mut in5, mut in7) = (0u64, 0u64, false, false);
        for line in std::io::BufReader::new(file).lines() {
            let Ok(line) = line else { break };
            // Cheap pre-filter: most lines carry no usage at all.
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
            if let Some(id) = d["message"]["id"].as_str() {
                if !seen.insert(id.to_string()) {
                    continue; // repeated content-block line or resumed-session copy
                }
            }
            let Some(ts) = line_ts(&d) else { continue };
            let toks = u["input_tokens"].as_u64().unwrap_or(0)
                + u["output_tokens"].as_u64().unwrap_or(0)
                + u["cache_creation_input_tokens"].as_u64().unwrap_or(0);
            let age = now.saturating_sub(ts);
            if age <= D7 {
                t7 += toks;
                in7 = true;
            }
            if age <= H5 {
                t5 += toks;
                in5 = true;
            }
        }
        b7.tokens += t7;
        b7.sessions += in7 as u64;
        b5.tokens += t5;
        b5.sessions += in5 as u64;
    }
    (b5, b7)
}

/// Codex: `payload.info.total_token_usage` is the monotonic running sum for the
/// session; the tokens attributable to one event are the DELTA from the
/// previous event, windowed by that line's timestamp.
fn codex_usage(dir: &Path, now: u64) -> (Bucket, Bucket) {
    let mut files = Vec::new();
    recent_jsonl(dir, now, D7, &mut files);
    let (mut b5, mut b7) = (Bucket::default(), Bucket::default());
    for f in &files {
        let Ok(file) = std::fs::File::open(f) else {
            continue;
        };
        let (mut t5, mut t7, mut in5, mut in7) = (0u64, 0u64, false, false);
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
            let age = now.saturating_sub(ts);
            if age <= D7 {
                t7 += delta;
                in7 = true;
            }
            if age <= H5 {
                t5 += delta;
                in5 = true;
            }
        }
        b7.tokens += t7;
        b7.sessions += in7 as u64;
        b5.tokens += t5;
        b5.sessions += in5 as u64;
    }
    (b5, b7)
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
        let (b5, b7) = claude_usage(dir.path(), now);
        assert_eq!(
            b5.tokens, 150,
            "duplicate lines and resumed copies count once"
        );
        assert_eq!(b7.tokens, 150, "the 7d-old message is excluded");
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
        let (b5, b7) = codex_usage(dir.path(), now);
        assert_eq!(b5.tokens, 1800, "cumulative total, duplicates ignored");
        assert_eq!(b7.tokens, 1800);
        assert_eq!(b5.sessions, 1);
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
        let (b5, b7) = codex_usage(dir.path(), now);
        assert_eq!(b5.tokens, 500, "only the in-window delta");
        assert_eq!(b7.tokens, 1500);
    }
}
