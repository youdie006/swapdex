//! Local, no-network usage read. Sums tokens from each tool's on-disk session
//! transcripts over recent windows (5h / 7d) so you can tell how heavily you
//! have been using an account and when to switch. Reads `~/.claude/projects`
//! and `~/.codex/sessions` only - never the network.
//!
//! This is a rough local estimate of activity, not the vendor's billed quota:
//! transcripts are not tagged by account, so it reflects this machine's usage
//! for each tool, not a per-account remaining balance.

use crate::paths::Paths;
use serde_json::Value;
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

/// Claude: each assistant message carries its own `message.usage` (one API
/// call), so sum per message and window by the message timestamp.
fn claude_usage(dir: &Path, now: u64) -> (Bucket, Bucket) {
    let mut files = Vec::new();
    recent_jsonl(dir, now, D7, &mut files);
    let (mut b5, mut b7) = (Bucket::default(), Bucket::default());
    for f in &files {
        let (mut t5, mut t7, mut in5, mut in7) = (0u64, 0u64, false, false);
        if let Ok(text) = std::fs::read_to_string(f) {
            for line in text.lines() {
                let d: Value = match serde_json::from_str(line) {
                    Ok(v) => v,
                    Err(_) => continue,
                };
                let u = &d["message"]["usage"];
                if !u.is_object() {
                    continue;
                }
                let ts = match d["timestamp"]
                    .as_str()
                    .and_then(crate::session_link::rfc3339_to_secs)
                {
                    Some(t) if t >= 0 => t as u64,
                    _ => continue,
                };
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
        }
        b7.tokens += t7;
        b7.sessions += in7 as u64;
        b5.tokens += t5;
        b5.sessions += in5 as u64;
    }
    (b5, b7)
}

/// Codex: `payload.info.last_token_usage` is cumulative within a session, so the
/// session total is its final value; window by the file's mtime.
fn codex_usage(dir: &Path, now: u64) -> (Bucket, Bucket) {
    let mut files = Vec::new();
    recent_jsonl(dir, now, D7, &mut files);
    let (mut b5, mut b7) = (Bucket::default(), Bucket::default());
    for f in &files {
        let age = now.saturating_sub(mtime_secs(f));
        let mut total = 0u64;
        if let Ok(text) = std::fs::read_to_string(f) {
            for line in text.lines() {
                let d: Value = match serde_json::from_str(line) {
                    Ok(v) => v,
                    Err(_) => continue,
                };
                let ltu = &d["payload"]["info"]["last_token_usage"];
                if ltu.is_object() {
                    let t = ltu["input_tokens"].as_u64().unwrap_or(0)
                        + ltu["output_tokens"].as_u64().unwrap_or(0);
                    if t > 0 {
                        total = t; // cumulative - keep the latest
                    }
                }
            }
        }
        if age <= D7 {
            b7.tokens += total;
            b7.sessions += 1;
        }
        if age <= H5 {
            b5.tokens += total;
            b5.sessions += 1;
        }
    }
    (b5, b7)
}

/// Human-friendly token count (e.g. 1.2M, 45.0k, 900).
pub fn human(n: u64) -> String {
    if n >= 1_000_000 {
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
    }

    #[test]
    fn claude_sums_recent_message_tokens() {
        // Fixed `now` + fixed timestamps -> deterministic windowing.
        let now = crate::session_link::rfc3339_to_secs("2026-07-06T12:00:00Z").unwrap() as u64;
        let dir = tempfile::tempdir().unwrap();
        let proj = dir.path().join("p");
        std::fs::create_dir_all(&proj).unwrap();
        let body = format!(
            "{}\n{}\n",
            serde_json::json!({"timestamp": "2026-07-06T11:59:00Z", "message": {"usage": {"input_tokens": 100, "output_tokens": 50, "cache_creation_input_tokens": 0}}}),
            serde_json::json!({"timestamp": "2026-06-01T00:00:00Z", "message": {"usage": {"input_tokens": 999, "output_tokens": 999}}}),
        );
        std::fs::write(proj.join("s.jsonl"), body).unwrap();
        // Force the file mtime into the 7d window so it's collected.
        let (b5, b7) = claude_usage(dir.path(), now);
        assert_eq!(b5.tokens, 150, "only the recent message counts in 5h");
        assert_eq!(b7.tokens, 150, "the 7d-old message is excluded");
        assert_eq!(b5.sessions, 1);
    }
}
