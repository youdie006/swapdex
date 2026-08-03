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
    /// Which tool's conversation this is - they are reopened differently.
    pub tool: &'static str,
}

impl Found {
    /// The one line that reopens this conversation, whatever account is active.
    /// Spelling out the config dir is what makes it work: the shim only fills
    /// that variable in when it is unset, so an explicit one always wins.
    pub fn resume_command(&self) -> String {
        // The REAL path, not the `~`-shortened one used for display: this line is
        // meant to be run, and a tilde inside quotes is not expanded - it becomes
        // a directory that does not exist, and the tool then reports no session,
        // which is the exact confusion this command exists to end.
        match self.tool {
            "codex" => format!(
                "CODEX_HOME={} codex resume {}",
                self.config_dir.display(),
                self.session_id
            ),
            _ => format!(
                "CLAUDE_CONFIG_DIR={} claude -r {}",
                self.config_dir.display(),
                self.session_id
            ),
        }
    }
}

/// One tool's stores and how its conversations are laid out.
///
/// Codex has the same problem Claude does - a conversation lives inside the home
/// the tool was launched with, so switching accounts changes which ones `resume`
/// can offer - but it files them by DATE rather than by project, with the working
/// directory recorded inside each transcript. So they are found by reading, not
/// by listing directories.
pub fn codex_stores(paths: &Paths) -> Vec<(String, PathBuf)> {
    let mut out: Vec<(String, PathBuf)> = crate::slots::Slots::open_for(paths, "codex")
        .map(|s| {
            s.list()
                .into_iter()
                .map(|r| (r.name, r.config_dir))
                .collect()
        })
        .unwrap_or_default();
    let bare = paths.codex_dir().to_path_buf();
    if !out.iter().any(|(_, d)| d == &bare) {
        out.push(("(default ~/.codex)".to_string(), bare));
    }
    out
}

/// The working directory a Codex transcript belongs to, from the transcript
/// itself. Only the head is read: `cwd` is recorded in the session's opening
/// lines, and these files run to megabytes.
fn codex_cwd(path: &Path) -> Option<String> {
    use std::io::Read;
    // The opening line is the session header and carries `cwd` about 150 bytes
    // in - but the whole line runs to 15KB, so parsing it as JSON needs all of
    // it. Reading a fixed head and parsing lines meant every transcript was cut
    // mid-line and silently skipped: the search found nothing, everywhere.
    let mut head = vec![0u8; 4096];
    let mut f = std::fs::File::open(path).ok()?;
    let n = f.read(&mut head).ok()?;
    cwd_in(&String::from_utf8_lossy(&head[..n]))
}

/// The `cwd` value out of a fragment of transcript, by scanning for the field
/// rather than parsing - the line it sits on is far longer than the fragment.
fn cwd_in(text: &str) -> Option<String> {
    let at = text.find("\"cwd\"")?;
    let rest = &text[at + 5..];
    let open = rest.find('"')?;
    let after = &rest[open + 1..];
    let close = after.find('"')?;
    let v = &after[..close];
    (!v.is_empty()).then(|| v.to_string())
}

/// Codex conversations across every account, newest first.
pub fn find_codex(paths: &Paths, project_filter: Option<&str>, limit: usize) -> Vec<Found> {
    let mut out = Vec::new();
    for (account, dir) in codex_stores(paths) {
        let mut files = Vec::new();
        collect_rollouts(&dir.join("sessions"), &mut files);
        // Newest first, so a filter that matches many still answers quickly.
        files.sort_by_key(|p| std::cmp::Reverse(mtime_secs(p)));
        for path in files.into_iter().take(400) {
            let Some(cwd) = codex_cwd(&path) else {
                continue;
            };
            if project_filter.is_some_and(|f| !matches_path(&cwd, f)) {
                continue;
            }
            let Some(id) = session_id_of(&path) else {
                continue;
            };
            out.push(Found {
                account: account.clone(),
                session_id: id,
                project: cwd,
                modified: mtime_secs(&path),
                config_dir: dir.clone(),
                tool: "codex",
            });
        }
    }
    out.sort_by_key(|f| std::cmp::Reverse(f.modified));
    out.truncate(limit);
    out
}

/// Codex names a transcript `rollout-<timestamp>-<uuid>.jsonl`; the id it
/// resumes by is the uuid at the end.
fn session_id_of(path: &Path) -> Option<String> {
    let stem = path.file_stem()?.to_string_lossy();
    stem.rsplit('-').next().filter(|s| s.len() >= 8).map(|s| {
        // The uuid is the last five dash-separated groups.
        let parts: Vec<&str> = stem.split('-').collect();
        if parts.len() >= 5 {
            parts[parts.len() - 5..].join("-")
        } else {
            s.to_string()
        }
    })
}

fn collect_rollouts(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return;
    };
    for e in rd.flatten() {
        let p = e.path();
        if p.is_dir() {
            collect_rollouts(&p, out);
        } else if p.extension().is_some_and(|x| x == "jsonl")
            && p.file_name()
                .is_some_and(|n| n.to_string_lossy().starts_with("rollout-"))
        {
            out.push(p);
        }
    }
}

/// A real filesystem path against what the user typed, both ways round.
fn matches_path(haystack: &str, needle: &str) -> bool {
    let h = haystack.to_ascii_lowercase();
    let n = needle.to_ascii_lowercase();
    h.contains(&n) || h.contains(&n.replace('/', "-"))
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
                tool: "claude-code",
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

    // A Codex transcript's opening line is the session header, and it runs to
    // kilobytes - so the `cwd` has to be read out of a fragment rather than by
    // parsing the line. Parsing meant every transcript was cut mid-line and
    // skipped, and the search found nothing at all.
    #[test]
    fn a_codex_working_directory_is_read_from_a_fragment() {
        let head = r#"{"timestamp":"2026-03-10T11:53:39Z","type":"session_meta","payload":{"id":"019cd798","cwd":"/Users/me/Project/ROS","originator":"codex_cli","#;
        assert_eq!(cwd_in(head).as_deref(), Some("/Users/me/Project/ROS"));
        // Truncated right after the field name, or before it: nothing claimed.
        assert!(cwd_in(r#"{"payload":{"id":"x","cwd""#).is_none());
        assert!(cwd_in(r#"{"payload":{"id":"x""#).is_none());
        // An empty value is not a directory.
        assert!(cwd_in(r#"{"cwd":""}"#).is_none());
    }

    // The id Codex resumes by is the uuid at the end of the file name, not the
    // timestamp in the middle of it.
    #[test]
    fn a_codex_session_id_is_the_uuid_in_its_name() {
        let p = Path::new("rollout-2026-03-10T20-53-35-019cd798-673e-7c31-bc00-6b91428a80c6.jsonl");
        assert_eq!(
            session_id_of(p).as_deref(),
            Some("019cd798-673e-7c31-bc00-6b91428a80c6")
        );
    }

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
        assert!(
            !cmd.contains('~'),
            "a path meant to be RUN cannot be tilde-shortened: {cmd}"
        );
        assert!(
            cmd.contains(&paths.claude_dir().display().to_string()),
            "it names the store in full: {cmd}"
        );
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
