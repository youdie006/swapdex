//! Which account holds a conversation.
//!
//! Claude keeps conversations inside the config dir it was launched with, so
//! switching accounts also switches which conversations `claude -c` and
//! `claude -r` can see. Nothing is lost when that happens, but nothing says so
//! either: the session is simply not in the store being looked at, and the
//! honest report ("no conversation found") reads as "your work is gone".
//!
//! This answers it from the filesystem rather than by inference: every account's
//! store is looked in directly, so the answer holds even for a session that
//! predates any switch swapdex recorded.

use crate::paths::Paths;
use std::path::{Path, PathBuf};

/// One conversation, and the account whose store holds it.
#[derive(Clone, Debug, PartialEq)]
pub struct Found {
    /// The account name, or the bare home when the store is not a slot.
    pub account: String,
    pub session_id: String,
    /// The working directory the conversation belongs to, as Claude encodes it.
    pub project: String,
    /// Unix seconds of the last write - conversations sort newest first.
    pub modified: u64,
    /// The store it lives in, for the exact command that reopens it.
    pub config_dir: PathBuf,
}

impl Found {
    /// The one line that reopens this conversation, whatever account is active.
    /// Spelling out the config dir is what makes it work: the shim only fills
    /// that variable in when it is unset, so an explicit one always wins.
    pub fn resume_command(&self) -> String {
        format!(
            "CLAUDE_CONFIG_DIR={} claude -r {}",
            crate::util::redact_path(&self.config_dir.display().to_string()),
            self.session_id
        )
    }
}

/// Every store swapdex knows about: the registered Claude slots, plus the bare
/// home, which holds the conversations from before any account was registered.
pub fn stores(paths: &Paths) -> Vec<(String, PathBuf)> {
    let mut out: Vec<(String, PathBuf)> = crate::slots::Slots::open_for(paths, "claude-code")
        .map(|s| {
            s.list()
                .into_iter()
                .map(|r| (r.name, r.config_dir))
                .collect()
        })
        .unwrap_or_default();
    let bare = paths.claude_dir().to_path_buf();
    if !out.iter().any(|(_, d)| d == &bare) {
        out.push(("(default ~/.claude)".to_string(), bare));
    }
    out
}

/// Conversations across every account, newest first. `project_filter` matches a
/// substring of the encoded project path; `None` searches all of them.
pub fn find(paths: &Paths, project_filter: Option<&str>, limit: usize) -> Vec<Found> {
    let mut out = Vec::new();
    for (account, dir) in stores(paths) {
        collect_store(&account, &dir, project_filter, &mut out);
    }
    out.sort_by_key(|f| std::cmp::Reverse(f.modified));
    out.truncate(limit);
    out
}

fn collect_store(account: &str, dir: &Path, filter: Option<&str>, out: &mut Vec<Found>) {
    let Ok(projects) = std::fs::read_dir(dir.join("projects")) else {
        return;
    };
    for p in projects.flatten() {
        let project = p.file_name().to_string_lossy().to_string();
        if filter.is_some_and(|f| !matches_project(&project, f)) {
            continue;
        }
        let Ok(files) = std::fs::read_dir(p.path()) else {
            continue;
        };
        for f in files.flatten() {
            let path = f.path();
            if path.extension().is_none_or(|e| e != "jsonl") {
                continue;
            }
            let Some(session_id) = path.file_stem().map(|s| s.to_string_lossy().to_string()) else {
                continue;
            };
            out.push(Found {
                account: account.to_string(),
                session_id,
                project: project.clone(),
                modified: mtime_secs(&path),
                config_dir: dir.to_path_buf(),
            });
        }
    }
}

/// Claude encodes a working directory by replacing every separator with `-`, so
/// a user searching for "Project/ROS" (or "ros") has to match that spelling too.
fn matches_project(encoded: &str, needle: &str) -> bool {
    let hay = encoded.to_ascii_lowercase();
    let n = needle.to_ascii_lowercase().replace(['/', '_'], "-");
    hay.contains(n.trim_matches('-'))
}

fn mtime_secs(p: &Path) -> u64 {
    std::fs::metadata(p)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_session(dir: &Path, project: &str, id: &str) {
        let d = dir.join("projects").join(project);
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(d.join(format!("{id}.jsonl")), b"{}\n").unwrap();
    }

    #[test]
    fn a_conversation_is_found_in_whichever_account_holds_it() {
        let root = tempfile::tempdir().unwrap();
        let paths = Paths::rooted(root.path());
        // The bare home holds the one the user is looking for; a slot holds
        // another project entirely.
        write_session(paths.claude_dir(), "-Users-me-Project-ROS", "sess-ros");
        let slot = root.path().join("company");
        write_session(&slot, "-Users-me-other", "sess-other");
        std::fs::create_dir_all(paths.store_dir()).unwrap();
        std::fs::write(
            paths.store_dir().join("slots.json"),
            serde_json::to_vec(&serde_json::json!([{
                "name": "work", "id": "i1", "config_dir": slot, "adopted": true,
                "tool": "claude-code"
            }]))
            .unwrap(),
        )
        .unwrap();

        let all = find(&paths, None, 10);
        assert_eq!(all.len(), 2, "both stores are searched: {all:?}");

        let ros = find(&paths, Some("Project/ROS"), 10);
        assert_eq!(ros.len(), 1, "a path spelled the human way still matches");
        assert_eq!(ros[0].session_id, "sess-ros");
        assert_eq!(
            ros[0].account, "(default ~/.claude)",
            "and it names the account whose store has it"
        );
        // The command names the store, because the shim only fills that variable
        // in when it is unset - an explicit one always wins.
        let cmd = ros[0].resume_command();
        assert!(cmd.contains("CLAUDE_CONFIG_DIR="), "{cmd}");
        assert!(cmd.ends_with("claude -r sess-ros"), "{cmd}");

        // A slot's own conversation is attributed to that account by name.
        let other = find(&paths, Some("other"), 10);
        assert_eq!(other.len(), 1);
        assert_eq!(other[0].account, "work");
    }

    #[test]
    fn a_store_with_no_conversations_is_simply_empty() {
        let root = tempfile::tempdir().unwrap();
        let paths = Paths::rooted(root.path());
        assert!(find(&paths, None, 10).is_empty(), "no stores, no results");
        // A projects dir that exists but holds nothing readable is not an error.
        std::fs::create_dir_all(paths.claude_dir().join("projects")).unwrap();
        assert!(find(&paths, None, 10).is_empty());
        // A stray non-jsonl file is ignored rather than reported as a session.
        let d = paths.claude_dir().join("projects").join("-p");
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(d.join("notes.txt"), b"x").unwrap();
        assert!(find(&paths, None, 10).is_empty());
    }
}
