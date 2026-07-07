//! The profile store at ~/.local/share/swapdex: named snapshots, a switch
//! timeline, an active-name hint, a cross-process lock, and bounded backups.
//! Everything is 0600, the store dir 0700; it holds plaintext refresh tokens and
//! is single-machine, single-user - never sync it.

use crate::adapters::Snapshot;
use crate::paths::Paths;
use crate::secret::Secret;
use anyhow::{Context, Result};
use fs2::FileExt;
use std::fs;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::PathBuf;

pub struct Store {
    dir: PathBuf,
}

pub struct ProfileInfo {
    pub name: String,
    pub tools: Vec<String>,
}

/// Holds an exclusive flock for its lifetime (released on drop). The file is
/// kept only to keep the lock; it is intentionally never read.
pub struct LockGuard(#[allow(dead_code)] fs::File);

/// chmod 0700/0600 everything under `dir` (dirs/files), best-effort.
fn tighten_tree(dir: &std::path::Path) {
    let Ok(rd) = fs::read_dir(dir) else { return };
    for e in rd.flatten() {
        let p = e.path();
        if p.is_dir() {
            fs::set_permissions(&p, fs::Permissions::from_mode(0o700)).ok();
            tighten_tree(&p);
        } else {
            fs::set_permissions(&p, fs::Permissions::from_mode(0o600)).ok();
        }
    }
}

impl Store {
    pub fn open(paths: &Paths) -> Result<Store> {
        let dir = paths.store_dir();
        fs::create_dir_all(&dir).with_context(|| format!("create store {}", dir.display()))?;
        // 0700 explicitly - do not rely on inherited perms.
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o700)).ok();
        for sub in ["accounts", "backups"] {
            let d = dir.join(sub);
            fs::create_dir_all(&d).ok();
            fs::set_permissions(&d, fs::Permissions::from_mode(0o700)).ok();
            // Snapshots ARE tokens: tighten everything under them too. cp -r,
            // backup tools, or a loose umask can widen modes after the fact,
            // and doctor's top-level check would miss it. Best-effort, tiny
            // tree (profiles x tools), runs on every open.
            tighten_tree(&d);
        }
        Ok(Store { dir })
    }

    /// Exclusive lock around the read-current -> backup -> apply compound; refuse
    /// (rather than block) if another swapdex is mid-switch.
    pub fn lock(&self) -> Result<LockGuard> {
        let path = self.dir.join(".lock");
        let f = fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .open(&path)
            .context("open store lock")?;
        f.try_lock_exclusive()
            .context("another swapdex is mid-switch (store is locked)")?;
        Ok(LockGuard(f))
    }

    fn account_tool_dir(&self, name: &str, tool: &str) -> PathBuf {
        self.dir.join("accounts").join(name).join(tool)
    }

    pub fn save(&self, name: &str, snap: &Snapshot) -> Result<()> {
        let d = self.account_tool_dir(name, snap.tool);
        fs::create_dir_all(&d).ok();
        fs::set_permissions(
            self.dir.join("accounts").join(name),
            fs::Permissions::from_mode(0o700),
        )
        .ok();
        fs::set_permissions(&d, fs::Permissions::from_mode(0o700)).ok();
        for (part, secret) in &snap.blobs {
            crate::atomic::write_secret(&d.join(part), secret.expose())?;
        }
        Ok(())
    }

    pub fn load(&self, name: &str, tool: &str) -> Result<Option<Snapshot>> {
        let d = self.account_tool_dir(name, tool);
        if !d.exists() {
            return Ok(None);
        }
        let tool_static: &'static str = match tool {
            "claude-code" => "claude-code",
            "codex" => "codex",
            "gemini" => "gemini",
            "antigravity" => "antigravity",
            _ => return Ok(None),
        };
        let mut blobs = Vec::new();
        for e in fs::read_dir(&d)?.flatten() {
            let part = e.file_name().to_string_lossy().into_owned();
            // Skip a transient ".<name>.swapdex.tmp" from a concurrent write so
            // it is never mistaken for a snapshot part.
            if e.path().is_file() && !part.starts_with('.') {
                let bytes = crate::atomic::read_regular(&e.path())?;
                blobs.push((part, Secret::new(bytes)));
            }
        }
        Ok(Some(Snapshot {
            tool: tool_static,
            blobs,
        }))
    }

    pub fn list(&self) -> Vec<ProfileInfo> {
        let mut out = Vec::new();
        let accounts = self.dir.join("accounts");
        if let Ok(rd) = fs::read_dir(&accounts) {
            for e in rd.flatten() {
                if !e.path().is_dir() {
                    continue;
                }
                let name = e.file_name().to_string_lossy().into_owned();
                let mut tools = Vec::new();
                if let Ok(td) = fs::read_dir(e.path()) {
                    for t in td.flatten() {
                        let tname = t.file_name().to_string_lossy().into_owned();
                        // Only KNOWN tools count - a stray subdir (crash
                        // debris, manual poking) must not render as a tool.
                        if t.path().is_dir() && KNOWN_TOOLS.contains(&tname.as_str()) {
                            tools.push(tname);
                        }
                    }
                }
                tools.sort();
                // An empty dir is not a profile: `use` would refuse it, so
                // `ls` showing it is a lie.
                if tools.is_empty() {
                    continue;
                }
                out.push(ProfileInfo { name, tools });
            }
        }
        out.sort_by(|a, b| a.name.cmp(&b.name));
        out
    }

    pub fn remove(&self, name: &str) -> Result<bool> {
        let d = self.dir.join("accounts").join(name);
        if !d.exists() {
            return Ok(false);
        }
        // Best-effort overwrite of snapshot bytes before unlinking (CoW caveat
        // documented in the README).
        overwrite_tree(&d);
        fs::remove_dir_all(&d).with_context(|| format!("remove profile {name}"))?;
        Ok(true)
    }

    /// Rename a profile. Returns false if `old` does not exist; errors if `new`
    /// already exists.
    /// Whether ANY directory (even a ghost one hidden from `list()`) claims
    /// this name - the collision test for rename targets.
    pub fn profile_dir_exists(&self, name: &str) -> bool {
        self.dir.join("accounts").join(name).exists()
    }

    pub fn rename(&self, old: &str, new: &str) -> Result<bool> {
        let from = self.dir.join("accounts").join(old);
        let to = self.dir.join("accounts").join(new);
        if !from.exists() {
            return Ok(false);
        }
        if to.exists() {
            anyhow::bail!("a profile named '{new}' already exists");
        }
        fs::rename(&from, &to).with_context(|| format!("rename profile {old} -> {new}"))?;
        // The timeline attributes sessions/usage by profile NAME - leaving the
        // old name there makes `usage`/`sessions` report a profile that no
        // longer exists, forever. Rewrite events in place (atomic).
        let tl = self.dir.join("timeline.jsonl");
        if let Ok(text) = fs::read_to_string(&tl) {
            let mut changed = false;
            let rewritten: Vec<String> = text
                .lines()
                .map(
                    |line| match serde_json::from_str::<serde_json::Value>(line) {
                        Ok(mut v) if v["account"] == old => {
                            v["account"] = serde_json::Value::String(new.to_string());
                            changed = true;
                            serde_json::to_string(&v).unwrap_or_else(|_| line.to_string())
                        }
                        _ => line.to_string(),
                    },
                )
                .collect();
            if changed {
                let mut out = rewritten.join("\n");
                out.push('\n');
                crate::atomic::write_secret(&tl, out.as_bytes())?;
            }
        }
        Ok(true)
    }

    /// Back up a live snapshot before a switch; keep only the newest 2 per tool.
    pub fn backup(&self, snap: &Snapshot) -> Result<()> {
        let base = self.dir.join("backups").join(snap.tool);
        fs::create_dir_all(&base).ok();
        fs::set_permissions(&base, fs::Permissions::from_mode(0o700)).ok();
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let d = base.join(stamp.to_string());
        fs::create_dir_all(&d).ok();
        fs::set_permissions(&d, fs::Permissions::from_mode(0o700)).ok();
        for (part, secret) in &snap.blobs {
            crate::atomic::write_secret(&d.join(part), secret.expose())?;
        }
        // Prune to newest 2.
        let mut stamps: Vec<PathBuf> = fs::read_dir(&base)
            .into_iter()
            .flatten()
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.is_dir())
            .collect();
        // Sort by the numeric stamp, not lexically, so pruning always drops the
        // genuinely-oldest backup (lexical sort would misorder across a digit-
        // length change).
        stamps.sort_by_key(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .and_then(|s| s.parse::<u128>().ok())
                .unwrap_or(0)
        });
        while stamps.len() > 2 {
            let old = stamps.remove(0);
            overwrite_tree(&old);
            let _ = fs::remove_dir_all(&old);
        }
        Ok(())
    }

    /// The newest backup snapshot for a tool (taken by `use` before each switch),
    /// with its unix-nanos stamp. `None` when no backup exists.
    pub fn load_backup(&self, tool: &str) -> Result<Option<(u128, Snapshot)>> {
        let tool_static: &'static str = match tool {
            "claude-code" => "claude-code",
            "codex" => "codex",
            "gemini" => "gemini",
            "antigravity" => "antigravity",
            _ => return Ok(None),
        };
        let base = self.dir.join("backups").join(tool);
        let mut stamps: Vec<(u128, PathBuf)> = fs::read_dir(&base)
            .into_iter()
            .flatten()
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.is_dir())
            .filter_map(|p| {
                let s = p.file_name()?.to_str()?.parse::<u128>().ok()?;
                Some((s, p))
            })
            .collect();
        stamps.sort_by_key(|(s, _)| *s);
        // Newest first, but skip a torn candidate (a crash between mkdir and
        // the blob writes leaves an empty/partial dir) - an older intact backup
        // is better than "no backup".
        while let Some((stamp, d)) = stamps.pop() {
            let mut blobs = Vec::new();
            for e in fs::read_dir(&d)?.flatten() {
                let part = e.file_name().to_string_lossy().into_owned();
                if e.path().is_file() && !part.starts_with('.') {
                    let bytes = crate::atomic::read_regular(&e.path())?;
                    blobs.push((part, Secret::new(bytes)));
                }
            }
            let complete = match tool_static {
                // A claude backup needs both parts or apply() will refuse it.
                "claude-code" => {
                    blobs.iter().any(|(n, _)| n == "credentials")
                        && blobs.iter().any(|(n, _)| n == "oauth_account")
                }
                _ => !blobs.is_empty(),
            };
            if !complete {
                continue;
            }
            return Ok(Some((
                stamp,
                Snapshot {
                    tool: tool_static,
                    blobs,
                },
            )));
        }
        Ok(None)
    }

    pub fn append_timeline(&self, tool: &str, account: &str, action: &str) -> Result<()> {
        let ts = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        self.append_timeline_at(tool, account, action, ts)
    }

    /// Append with an explicit timestamp so every tool touched by ONE `use` /
    /// `restore` invocation shares the same ts - that shared ts is what lets a
    /// bare `restore` scope itself to exactly the last switch.
    pub fn append_timeline_at(
        &self,
        tool: &str,
        account: &str,
        action: &str,
        ts: u64,
    ) -> Result<()> {
        let path = self.dir.join("timeline.jsonl");
        let line =
            serde_json::json!({"ts": ts, "tool": tool, "account": account, "action": action});
        let mut buf = if path.exists() {
            crate::atomic::read_regular(&path)?
        } else {
            Vec::new()
        };
        buf.extend_from_slice(serde_json::to_string(&line)?.as_bytes());
        buf.push(b'\n');
        // Bound the file: session attribution only needs recent history, so
        // compact to the newest TIMELINE_KEEP events once it doubles that.
        const TIMELINE_KEEP: usize = 1000;
        let lines = buf.iter().filter(|&&b| b == b'\n').count();
        if lines > TIMELINE_KEEP * 2 {
            let text = String::from_utf8_lossy(&buf).into_owned();
            let tail: Vec<&str> = text
                .lines()
                .rev()
                .take(TIMELINE_KEEP)
                .collect::<Vec<_>>()
                .into_iter()
                .rev()
                .collect();
            buf = (tail.join("\n") + "\n").into_bytes();
        }
        crate::atomic::write_secret(&path, &buf)
    }
}

/// A profile name must be a single safe path component - reject anything that
/// could escape the store (`/`, `\`, `..`, a leading `.`, control chars, empty,
/// or absurdly long). Guards `add`/`use`/`rm`/`rename` against path traversal.
pub const KNOWN_TOOLS: [&str; 4] = ["claude-code", "codex", "gemini", "antigravity"];

pub fn valid_profile_name(name: &str) -> bool {
    // NOTE: "-" is reserved at CREATION time (add/rename reject it) because
    // `use -` toggles - but it stays valid here so a legacy profile named "-"
    // can still be rm'd/renamed after an upgrade.
    !name.trim().is_empty()
        && name.len() <= 64
        && !name.starts_with('.')
        && !name.contains(['/', '\\'])
        && !name.chars().any(|c| c.is_control())
}

fn overwrite_tree(dir: &std::path::Path) {
    if let Ok(rd) = fs::read_dir(dir) {
        for e in rd.flatten() {
            let p = e.path();
            if p.is_dir() {
                overwrite_tree(&p);
            } else if let Ok(len) = fs::metadata(&p).map(|m| m.len()) {
                let _ = fs::write(&p, vec![0u8; len as usize]);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::secret::Secret;

    fn snap() -> Snapshot {
        Snapshot {
            tool: "codex",
            blobs: vec![("auth".into(), Secret::new(b"{\"k\":\"SENTINEL\"}".to_vec()))],
        }
    }

    fn walk_files(dir: &std::path::Path) -> Vec<PathBuf> {
        let mut out = vec![];
        if let Ok(rd) = fs::read_dir(dir) {
            for e in rd.flatten() {
                let p = e.path();
                if p.is_dir() {
                    out.extend(walk_files(&p));
                } else {
                    out.push(p);
                }
            }
        }
        out
    }

    #[test]
    fn store_dir_is_0700_and_roundtrips_a_snapshot() {
        let d = tempfile::tempdir().unwrap();
        let p = Paths::rooted(d.path());
        let s = Store::open(&p).unwrap();
        assert_eq!(
            fs::metadata(p.store_dir()).unwrap().permissions().mode() & 0o777,
            0o700
        );
        s.save("work", &snap()).unwrap();
        let back = s.load("work", "codex").unwrap().unwrap();
        assert_eq!(back.part("auth").unwrap().expose(), b"{\"k\":\"SENTINEL\"}");
        for f in walk_files(&p.store_dir().join("accounts/work")) {
            assert_eq!(
                fs::metadata(&f).unwrap().permissions().mode() & 0o777,
                0o600,
                "{f:?}"
            );
        }
    }

    #[test]
    fn timeline_holds_no_secret() {
        let d = tempfile::tempdir().unwrap();
        let p = Paths::rooted(d.path());
        let s = Store::open(&p).unwrap();
        s.append_timeline("codex", "work", "use").unwrap();
        let tl = fs::read_to_string(p.store_dir().join("timeline.jsonl")).unwrap();
        assert!(!tl.contains("SENTINEL"));
        assert!(tl.contains("work") && tl.contains("codex"));
    }

    #[test]
    fn lock_is_exclusive() {
        let d = tempfile::tempdir().unwrap();
        let p = Paths::rooted(d.path());
        let s = Store::open(&p).unwrap();
        let _g = s.lock().unwrap();
        assert!(s.lock().is_err(), "second lock must fail while held");
    }
}
