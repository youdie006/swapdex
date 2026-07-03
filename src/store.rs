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

/// Holds an exclusive flock for its lifetime (released on drop).
pub struct LockGuard(fs::File);

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
            _ => return Ok(None),
        };
        let mut blobs = Vec::new();
        for e in fs::read_dir(&d)?.flatten() {
            if e.path().is_file() {
                let part = e.file_name().to_string_lossy().into_owned();
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
                        if t.path().is_dir() {
                            tools.push(t.file_name().to_string_lossy().into_owned());
                        }
                    }
                }
                tools.sort();
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
        stamps.sort();
        while stamps.len() > 2 {
            let old = stamps.remove(0);
            overwrite_tree(&old);
            let _ = fs::remove_dir_all(&old);
        }
        Ok(())
    }

    pub fn append_timeline(&self, tool: &str, account: &str, action: &str) -> Result<()> {
        let path = self.dir.join("timeline.jsonl");
        let ts = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let line =
            serde_json::json!({"ts": ts, "tool": tool, "account": account, "action": action});
        let mut buf = if path.exists() {
            crate::atomic::read_regular(&path)?
        } else {
            Vec::new()
        };
        buf.extend_from_slice(serde_json::to_string(&line)?.as_bytes());
        buf.push(b'\n');
        crate::atomic::write_secret(&path, &buf)
    }

    pub fn set_active(&self, tool: &str, name: &str) -> Result<()> {
        let path = self.dir.join("active.json");
        let mut map: serde_json::Value = if path.exists() {
            serde_json::from_slice(&crate::atomic::read_regular(&path)?).unwrap_or_default()
        } else {
            serde_json::json!({})
        };
        map[tool] = serde_json::json!(name);
        crate::atomic::write_secret(&path, &serde_json::to_vec(&map)?)
    }

    pub fn active(&self, tool: &str) -> Option<String> {
        let path = self.dir.join("active.json");
        let map: serde_json::Value =
            serde_json::from_slice(&crate::atomic::read_regular(&path).ok()?).ok()?;
        map[tool].as_str().map(|s| s.to_string())
    }
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
    fn timeline_and_active_hold_no_secret() {
        let d = tempfile::tempdir().unwrap();
        let p = Paths::rooted(d.path());
        let s = Store::open(&p).unwrap();
        s.append_timeline("codex", "work", "use").unwrap();
        s.set_active("codex", "work").unwrap();
        let tl = fs::read_to_string(p.store_dir().join("timeline.jsonl")).unwrap();
        let ac = fs::read_to_string(p.store_dir().join("active.json")).unwrap();
        assert!(!tl.contains("SENTINEL") && !ac.contains("SENTINEL"));
        assert_eq!(s.active("codex").as_deref(), Some("work"));
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
