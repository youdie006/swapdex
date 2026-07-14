//! Recent sessions read STRAIGHT from each tool's own on-disk store - no
//! sessionwiki required. Claude Code: `~/.claude/projects/**/<uuid>.jsonl`
//! (line objects carry `cwd` and the user messages). Codex:
//! `~/.codex/sessions/**/rollout-<ts>-<uuid>.jsonl`. Resume uses each tool's
//! native mechanism (`claude --resume <id>` in the session's cwd,
//! `codex resume <id>`), the same semantics sessionwiki uses.
//!
//! A session's recorded cwd is self-asserted data from the file; it is only
//! trusted when it exists as a real local directory (launching a tool in an
//! attacker-chosen dir would load that dir's CLAUDE.md/.mcp.json).

use crate::paths::Paths;
use std::path::{Path, PathBuf};

pub struct NativeSession {
    pub tool: &'static str,
    pub id: String,
    pub title: String,
    pub cwd: Option<PathBuf>,
    /// Unix seconds (file mtime - good enough for ordering and attribution).
    pub started: i64,
}

fn mtime_secs(p: &Path) -> i64 {
    std::fs::metadata(p)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

fn looks_like_uuid(s: &str) -> bool {
    s.len() == 36
        && s.chars().enumerate().all(|(i, c)| match i {
            8 | 13 | 18 | 23 => c == '-',
            _ => c.is_ascii_hexdigit(),
        })
}

/// First 64KB of a file as lines - all the head-parsing below is bounded.
fn head_lines(p: &Path) -> Vec<String> {
    use std::io::Read;
    let Ok(mut f) = std::fs::File::open(p) else {
        return Vec::new();
    };
    let mut buf = vec![0u8; 64 * 1024];
    let n = f.read(&mut buf).unwrap_or(0);
    buf.truncate(n);
    String::from_utf8_lossy(&buf)
        .lines()
        .map(|l| l.to_string())
        .collect()
}

fn first_text(v: &serde_json::Value) -> Option<String> {
    if let Some(s) = v.as_str() {
        return Some(s.to_string());
    }
    if let Some(arr) = v.as_array() {
        for item in arr {
            if let Some(t) = item["text"].as_str() {
                return Some(t.to_string());
            }
        }
    }
    None
}

fn tidy_title(t: &str) -> String {
    let one = t.split_whitespace().collect::<Vec<_>>().join(" ");
    let head: String = one.chars().take(60).collect();
    if head.is_empty() {
        "(no prompt)".into()
    } else {
        head
    }
}

/// Claude session file -> (title from the first user message, cwd).
fn claude_head(p: &Path) -> (String, Option<PathBuf>) {
    let mut title = None;
    let mut cwd = None;
    for line in head_lines(p) {
        let Ok(v) = serde_json::from_str::<serde_json::Value>(&line) else {
            continue;
        };
        if cwd.is_none() {
            if let Some(c) = v["cwd"].as_str() {
                let pb = PathBuf::from(c);
                if pb.is_dir() {
                    cwd = Some(pb); // only a real local dir is trusted
                }
            }
        }
        if title.is_none() && v["type"] == "user" && !v["isMeta"].as_bool().unwrap_or(false) {
            if let Some(t) = first_text(&v["message"]["content"]) {
                // Skip only KNOWN harness-injected payloads - a real prompt
                // may legitimately start with '<' ("<table> won't render").
                let meta = t.starts_with("<command-")
                    || t.starts_with("<local-command-")
                    || t.starts_with("<system-")
                    || t.starts_with("<task-notification");
                if !meta {
                    title = Some(tidy_title(&t));
                }
            }
        }
        if title.is_some() && cwd.is_some() {
            break;
        }
    }
    (title.unwrap_or_else(|| "(no prompt)".into()), cwd)
}

/// Codex rollout file -> (title from the first user message, cwd).
/// Codex harness boilerplate that must not become a session title (mirrors
/// sessionwiki's codex adapter). The real first prompt follows it.
fn codex_boilerplate(t: &str) -> bool {
    let s = t.trim_start();
    s.starts_with("<user_instructions>")
        || s.starts_with("<environment_context>")
        || s.starts_with("<ENVIRONMENT_CONTEXT>")
        || s.starts_with("<turn_context>")
        || s.starts_with("# AGENTS.md instructions")
        || s.starts_with("<INSTRUCTIONS>")
}

fn codex_head(p: &Path) -> (String, Option<PathBuf>) {
    let mut title = None;
    let mut cwd = None;
    for line in head_lines(p) {
        let Ok(v) = serde_json::from_str::<serde_json::Value>(&line) else {
            continue;
        };
        let payload = &v["payload"];
        if cwd.is_none() {
            if let Some(c) = payload["cwd"].as_str() {
                let pb = PathBuf::from(c);
                if pb.is_dir() {
                    cwd = Some(pb);
                }
            }
        }
        if title.is_none() {
            // Current rollouts carry the first user prompt in one of three
            // shapes - read whichever comes first (older code only read the
            // response_item shape, so current sessions showed "(no prompt)"):
            //   event_msg/user_message : payload.message is a plain string
            //   response_item/message  : payload.role=="user", payload.content
            //   pre-payload (2025)     : bare {type:"message",role:"user",content}
            let candidate = if payload["type"] == "user_message" {
                payload["message"].as_str().map(str::to_string)
            } else if payload["role"] == "user" {
                first_text(&payload["content"])
            } else if v["type"] == "message" && v["role"] == "user" {
                first_text(&v["content"])
            } else {
                None
            };
            if let Some(t) = candidate {
                if !codex_boilerplate(&t) {
                    title = Some(tidy_title(&t));
                }
            }
        }
        if title.is_some() && cwd.is_some() {
            break;
        }
    }
    (title.unwrap_or_else(|| "(no prompt)".into()), cwd)
}

fn walk_jsonl(dir: &Path, out: &mut Vec<PathBuf>) {
    if let Ok(rd) = std::fs::read_dir(dir) {
        for e in rd.flatten() {
            let p = e.path();
            if p.is_dir() {
                walk_jsonl(&p, out);
            } else if p.extension().is_some_and(|x| x == "jsonl") {
                out.push(p);
            }
        }
    }
}

/// The newest `n` sessions across Claude Code and Codex, straight from disk.
pub fn recent(paths: &Paths, n: usize) -> Vec<NativeSession> {
    let mut files: Vec<(i64, &'static str, PathBuf)> = Vec::new();
    let mut claude = Vec::new();
    walk_jsonl(&paths.claude_projects(), &mut claude);
    for p in claude {
        // Subagent transcripts resume via their parent; skip them in the menu.
        if p.to_string_lossy().contains("/subagents/") {
            continue;
        }
        files.push((mtime_secs(&p), "claude-code", p));
    }
    let mut codex = Vec::new();
    walk_jsonl(&paths.codex_sessions(), &mut codex);
    for p in codex {
        files.push((mtime_secs(&p), "codex", p));
    }
    files.sort_by_key(|(t, _, _)| std::cmp::Reverse(*t));
    files.truncate(n.max(16)); // parse only what the menu could ever need
    let mut out = Vec::new();
    for (started, tool, path) in files {
        let stem = match path.file_stem() {
            Some(s) => s.to_string_lossy().into_owned(),
            None => continue,
        };
        let id = match tool {
            "claude-code" => {
                if !looks_like_uuid(&stem) {
                    continue;
                }
                stem
            }
            _ => {
                // rollout-<timestamp>-<uuid>
                let Some(tail) = stem.len().checked_sub(36).and_then(|i| stem.get(i..)) else {
                    continue;
                };
                if !looks_like_uuid(tail) {
                    continue;
                }
                tail.to_string()
            }
        };
        let (title, cwd) = match tool {
            "claude-code" => claude_head(&path),
            _ => codex_head(&path),
        };
        out.push(NativeSession {
            tool,
            id,
            title,
            cwd,
            started,
        });
        if out.len() >= n {
            break;
        }
    }
    out
}

/// Replace this process with the tool's own resume flow. Only returns on
/// exec failure.
pub fn exec_resume(s: &NativeSession) -> anyhow::Error {
    use std::os::unix::process::CommandExt;
    let mut cmd = match s.tool {
        "claude-code" => {
            let mut c = std::process::Command::new("claude");
            c.args(["--resume", &s.id]);
            c
        }
        _ => {
            let mut c = std::process::Command::new("codex");
            c.args(["resume", &s.id]);
            c
        }
    };
    if let Some(d) = &s.cwd {
        cmd.current_dir(d);
    }
    let err = cmd.exec();
    anyhow::anyhow!("could not resume via {}: {err}", s.tool)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_claude_and_codex_sessions_from_disk() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::rooted(dir.path());
        let proj = dir.path().join(".claude/projects/-home-dev-api");
        std::fs::create_dir_all(&proj).unwrap();
        let cwd = dir.path().join("realdir");
        std::fs::create_dir_all(&cwd).unwrap();
        std::fs::write(
            proj.join("0a000000-0000-4000-8000-000000000001.jsonl"),
            format!(
                "{}\n{}\n",
                serde_json::json!({"type":"user","isMeta":true,"cwd":cwd.to_str().unwrap(),
                                   "message":{"content":"<boilerplate>"}}),
                serde_json::json!({"type":"user",
                                   "message":{"content":[{"type":"text","text":"fix the login redirect loop please"}]}}),
            ),
        )
        .unwrap();
        let cx = dir.path().join(".codex/sessions/2026/07/07");
        std::fs::create_dir_all(&cx).unwrap();
        std::fs::write(
            cx.join("rollout-2026-07-07T10-00-00-0a000000-0000-4000-8000-00000000000b.jsonl"),
            format!(
                "{}\n",
                serde_json::json!({"payload":{"role":"user","cwd":"/no/such/dir",
                                   "content":[{"type":"input_text","text":"tighten the retry budget"}]}}),
            ),
        )
        .unwrap();

        let got = recent(&paths, 10);
        assert_eq!(got.len(), 2);
        let claude = got.iter().find(|s| s.tool == "claude-code").unwrap();
        assert!(claude.title.contains("login redirect"), "{}", claude.title);
        assert_eq!(
            claude.cwd.as_deref(),
            Some(cwd.as_path()),
            "real dir trusted"
        );
        let codex = got.iter().find(|s| s.tool == "codex").unwrap();
        assert!(codex.title.contains("retry budget"));
        assert_eq!(codex.cwd, None, "nonexistent cwd is NOT trusted");
        assert_eq!(codex.id, "0a000000-0000-4000-8000-00000000000b");
    }

    // Current Codex rollouts carry the first prompt as event_msg/user_message
    // (a plain string), after an AGENTS.md boilerplate line. The older parser
    // read only the response_item shape, so these titled as "(no prompt)".
    #[test]
    fn codex_title_from_current_event_msg_format_skips_boilerplate() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::rooted(dir.path());
        let cx = dir.path().join(".codex/sessions/2026/07/14");
        std::fs::create_dir_all(&cx).unwrap();
        std::fs::write(
            cx.join("rollout-2026-07-14T09-00-00-0a000000-0000-4000-8000-0000000000cc.jsonl"),
            format!(
                "{}\n{}\n{}\n",
                serde_json::json!({"type":"session_meta","payload":{"id":"x"}}),
                serde_json::json!({"type":"event_msg","payload":{"type":"user_message",
                    "message":"# AGENTS.md instructions for /home/dev\n\n<INSTRUCTIONS>do x</INSTRUCTIONS>"}}),
                serde_json::json!({"type":"event_msg","payload":{"type":"user_message",
                    "message":"fix the flaky retry in the codex client"}}),
            ),
        )
        .unwrap();
        let got = recent(&paths, 10);
        let codex = got.iter().find(|s| s.tool == "codex").unwrap();
        assert!(
            codex.title.contains("flaky retry"),
            "reads the real prompt, not '(no prompt)' or the AGENTS.md line: {}",
            codex.title
        );
    }
}
