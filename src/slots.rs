//! The permanent-slot registry: a name -> slot mapping persisted to
//! `<store_dir>/slots.json`. Each slot is a directory under
//! `<store_dir>/slots/<id>/` handed to the tool as its own home - Claude's
//! `CLAUDE_CONFIG_DIR`, Codex's `CODEX_HOME`. swapdex never writes a credential
//! into a slot; the tool's own login does. The id is opaque and
//! name-independent so a rename never changes the directory (and therefore never
//! changes the Keychain service, which is derived from the dir string).
//!
//! A registry is opened FOR one tool and sees only that tool's slots. The same
//! account name on two tools is two accounts, and each tool has its own default
//! pointer, so switching Codex never moves where Claude launches.

use crate::paths::Paths;
use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SlotRecord {
    pub name: String,
    pub id: String,
    pub config_dir: PathBuf,
    #[serde(default)]
    pub adopted: bool,
    /// Which tool this slot is a home for. Absent in registries written before
    /// tools were distinguished, and those are Claude's - the only kind that
    /// existed - so the default keeps an upgraded install working untouched.
    #[serde(default = "default_tool")]
    pub tool: String,
}

fn default_tool() -> String {
    "claude-code".to_string()
}

pub struct Slots {
    file: PathBuf,
    slots_dir: PathBuf,
    /// Every slot on disk, of every tool: writes must preserve the tools this
    /// registry is not scoped to rather than dropping them.
    all: Vec<SlotRecord>,
    /// Just this tool's, in registry order.
    records: Vec<SlotRecord>,
    tool: String,
}

/// The environment variable a tool reads to find the home it should use.
pub fn home_var(tool: &str) -> Option<&'static str> {
    match tool {
        "claude-code" => Some("CLAUDE_CONFIG_DIR"),
        "codex" => Some("CODEX_HOME"),
        _ => None,
    }
}

/// 16 hex chars of sha256(name + a monotonic-ish nanosecond stamp) - opaque and
/// stable once created. Not derived from the name alone, so a rename is free.
fn new_id(name: &str) -> String {
    use sha2::{Digest, Sha256};
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let mut h = Sha256::new();
    h.update(name.as_bytes());
    h.update(nanos.to_le_bytes());
    h.finalize()
        .iter()
        .take(8)
        .map(|b| format!("{b:02x}"))
        .collect()
}

impl Slots {
    /// Claude's registry - the established caller, kept so every existing call
    /// site keeps meaning what it meant.
    pub fn open(paths: &Paths) -> Result<Slots> {
        Self::open_for(paths, "claude-code")
    }

    /// The registry as `tool` sees it.
    pub fn open_for(paths: &Paths, tool: &str) -> Result<Slots> {
        let store = paths.store_dir();
        let file = store.join("slots.json");
        let all: Vec<SlotRecord> = if file.exists() {
            let bytes = std::fs::read(&file).context("read slots.json")?;
            serde_json::from_slice(&bytes).context("slots.json is corrupt")?
        } else {
            Vec::new()
        };
        let records = all.iter().filter(|r| r.tool == tool).cloned().collect();
        Ok(Slots {
            file,
            slots_dir: store.join("slots"),
            all,
            records,
            tool: tool.to_string(),
        })
    }

    pub fn get(&self, name: &str) -> Option<SlotRecord> {
        self.records.iter().find(|r| r.name == name).cloned()
    }

    pub fn list(&self) -> Vec<SlotRecord> {
        self.records.clone()
    }

    pub fn create(&mut self, name: &str) -> Result<SlotRecord> {
        let name = name.trim();
        if name.is_empty() {
            bail!("a slot name is required");
        }
        if self.records.iter().any(|r| r.name == name) {
            bail!("a slot named '{name}' already exists");
        }
        let id = new_id(name);
        let config_dir = self.slots_dir.join(&id);
        std::fs::create_dir_all(&config_dir).context("create slot dir")?;
        let rec = SlotRecord {
            name: name.to_string(),
            id,
            config_dir,
            adopted: false,
            tool: self.tool.clone(),
        };
        self.records.push(rec.clone());
        self.persist()?;
        Ok(rec)
    }

    /// Rename a slot. Only the NAME changes: the id and directory stay, which is
    /// what keeps the macOS Keychain item (derived from the directory string)
    /// valid - renaming must never cost an account its login.
    pub fn rename(&mut self, old: &str, new: &str) -> Result<bool> {
        let new = new.trim();
        if new.is_empty() {
            bail!("a slot name is required");
        }
        if self.records.iter().any(|r| r.name == new) {
            bail!("an account named '{new}' already exists");
        }
        let Some(r) = self.records.iter_mut().find(|r| r.name == old) else {
            return Ok(false);
        };
        r.name = new.to_string();
        self.persist()?;
        Ok(true)
    }

    /// Unregister a slot: drop the name -> directory mapping. The DIRECTORY is
    /// left alone, always. It holds that account's login, and for an adopted dir
    /// it is somewhere the user chose - deleting either would turn "stop managing
    /// this" into "lose this account". Re-registering restores it.
    /// Returns false when there was no such slot.
    pub fn remove(&mut self, name: &str) -> Result<bool> {
        let Some(i) = self.records.iter().position(|r| r.name == name) else {
            return Ok(false);
        };
        let gone = self.records.remove(i);
        // A dangling default pointer would send a plain `claude` at a directory
        // swapdex no longer knows about; clear it when it named this slot.
        if self.default_dir().as_deref() == Some(gone.config_dir.as_path()) {
            let _ = std::fs::remove_file(self.pointer_file());
        }
        self.persist()?;
        Ok(true)
    }

    fn persist(&mut self) -> Result<()> {
        if let Some(parent) = self.file.parent() {
            std::fs::create_dir_all(parent).context("create store dir")?;
        }
        // One file holds every tool's slots. Writing only the scoped view would
        // delete the other tools' accounts, so the others are carried through
        // untouched and this tool's are replaced wholesale.
        let mut out: Vec<SlotRecord> = self
            .all
            .iter()
            .filter(|r| r.tool != self.tool)
            .cloned()
            .collect();
        out.extend(self.records.iter().cloned());
        let bytes = serde_json::to_vec_pretty(&out)?;
        std::fs::write(&self.file, bytes).context("write slots.json")?;
        self.all = out;
        Ok(())
    }

    /// Register an EXISTING config dir as a slot without creating or moving it
    /// (adoption of a `~/.claude-*` the user already uses). `config_dir` must be
    /// an existing absolute path.
    pub fn adopt(&mut self, name: &str, config_dir: &std::path::Path) -> Result<SlotRecord> {
        let name = name.trim();
        if name.is_empty() {
            bail!("a slot name is required");
        }
        if self.records.iter().any(|r| r.name == name) {
            bail!("a slot named '{name}' already exists");
        }
        if !config_dir.is_absolute() {
            bail!("config dir must be an absolute path");
        }
        if !config_dir.is_dir() {
            bail!("config dir does not exist: {}", config_dir.display());
        }
        let rec = SlotRecord {
            name: name.to_string(),
            id: new_id(name),
            config_dir: config_dir.to_path_buf(),
            adopted: true,
            tool: self.tool.clone(),
        };
        self.records.push(rec.clone());
        self.persist()?;
        Ok(rec)
    }

    /// The pointer file this tool's shim reads to find the default account's
    /// slot: `<store_dir>/active-claude`, `<store_dir>/active-codex`. Claude's
    /// keeps the name it has always had, so upgrading never loses a default.
    fn pointer_file(&self) -> PathBuf {
        // `self.file` is `<store_dir>/slots.json`; its parent is the store dir.
        let store = self
            .file
            .parent()
            .map(|p| p.to_path_buf())
            .unwrap_or_default();
        let short = match self.tool.as_str() {
            "claude-code" => "claude",
            other => other,
        };
        store.join(format!("active-{short}"))
    }

    /// Point the default account at `name`'s slot. A plain `claude` (via the
    /// shim) then launches in this slot. No credential is moved.
    pub fn set_default(&self, name: &str) -> Result<()> {
        let rec = self
            .get(name)
            .with_context(|| format!("no slot named '{name}'"))?;
        let p = self.pointer_file();
        if let Some(parent) = p.parent() {
            std::fs::create_dir_all(parent).context("create store dir")?;
        }
        std::fs::write(&p, rec.config_dir.to_string_lossy().as_bytes())
            .with_context(|| format!("write {} pointer", self.tool))?;
        Ok(())
    }

    /// The default account's slot dir, if a default has been set.
    pub fn default_dir(&self) -> Option<PathBuf> {
        let s = std::fs::read_to_string(self.pointer_file()).ok()?;
        let s = s.trim();
        (!s.is_empty()).then(|| PathBuf::from(s))
    }
}

/// The shared, account-agnostic config files symlinked from the bare home dir
/// into a freshly-created slot, so switching accounts does not change the user's
/// tooling. The token, history, and the file holding account identity stay
/// per-slot and are NOT linked. (For Claude, MCP config lives inside
/// `.claude.json`, which is per-account; sharing it needs the resolution noted
/// in the design's open questions, so it is intentionally left per-slot.)
pub const SHARED_CONFIG_FILES: &[&str] = &["settings.json", "CLAUDE.md"];

/// Codex keeps its settings in `config.toml` and its project instructions in
/// `AGENTS.md`; its credential lives apart in `auth.json`, which is what makes
/// the same split work. `sessions/` stays per-slot - it is that account's
/// history, and it is also where Codex records the rate limits swapdex reads.
pub const SHARED_CONFIG_FILES_CODEX: &[&str] = &["config.toml", "AGENTS.md"];

/// The files `tool` shares across its accounts.
pub fn shared_files(tool: &str) -> &'static [&'static str] {
    match tool {
        "codex" => SHARED_CONFIG_FILES_CODEX,
        _ => SHARED_CONFIG_FILES,
    }
}

/// Symlink the shared config files from `source` into `slot` (best-effort; skips
/// files absent in source or already present in the slot). Returns the names
/// linked.
pub fn link_shared_config(
    slot: &std::path::Path,
    source: &std::path::Path,
    tool: &str,
) -> Vec<String> {
    let mut linked = Vec::new();
    for name in shared_files(tool) {
        let src = source.join(name);
        let dst = slot.join(name);
        if src.exists() && !dst.exists() {
            #[cfg(unix)]
            if std::os::unix::fs::symlink(&src, &dst).is_ok() {
                linked.push((*name).to_string());
            }
        }
    }
    linked
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::paths::Paths;

    #[test]
    fn create_persists_and_reloads() {
        let root = tempfile::tempdir().unwrap();
        let paths = Paths::rooted(root.path());
        let rec = {
            let mut s = Slots::open(&paths).unwrap();
            s.create("work").unwrap()
        };
        // config_dir is absolute and under the store's slots dir; the dir exists.
        assert!(rec.config_dir.is_absolute());
        assert!(rec.config_dir.starts_with(paths.store_dir().join("slots")));
        assert!(rec.config_dir.is_dir(), "slot dir was created");
        assert!(!rec.adopted);
        // A fresh open sees it (persisted to slots.json).
        let s2 = Slots::open(&paths).unwrap();
        assert_eq!(s2.get("work").unwrap().id, rec.id);
        assert_eq!(s2.list().len(), 1);
    }

    // Codex isolates an account the same way Claude does - its own home dir via
    // CODEX_HOME - so the slot model has to hold both without either tool seeing
    // the other's accounts or moving the other's pointer.
    #[test]
    fn slots_are_scoped_to_their_tool() {
        let root = tempfile::tempdir().unwrap();
        let paths = Paths::rooted(root.path());
        {
            let mut c = Slots::open_for(&paths, "claude-code").unwrap();
            c.create("work").unwrap();
        }
        {
            // The SAME name on another tool is a different account, not a clash.
            let mut x = Slots::open_for(&paths, "codex").unwrap();
            x.create("work").unwrap();
        }
        let c = Slots::open_for(&paths, "claude-code").unwrap();
        let x = Slots::open_for(&paths, "codex").unwrap();
        assert_eq!(c.list().len(), 1, "claude sees only its own");
        assert_eq!(x.list().len(), 1, "codex sees only its own");
        assert_ne!(
            c.get("work").unwrap().config_dir,
            x.get("work").unwrap().config_dir,
            "two tools never share a directory"
        );
        // Pointers are independent: switching Codex must not move Claude.
        c.set_default("work").unwrap();
        assert_eq!(c.default_dir(), Some(c.get("work").unwrap().config_dir));
        assert_eq!(x.default_dir(), None, "codex has no default yet");
        x.set_default("work").unwrap();
        assert_eq!(c.default_dir(), Some(c.get("work").unwrap().config_dir));
        assert_eq!(x.default_dir(), Some(x.get("work").unwrap().config_dir));
    }

    // Slots registered before tools were distinguished are Claude's - that is the
    // only kind that existed - and must keep working across the upgrade.
    #[test]
    fn a_slot_recorded_without_a_tool_is_claudes() {
        let root = tempfile::tempdir().unwrap();
        let paths = Paths::rooted(root.path());
        std::fs::create_dir_all(paths.store_dir()).unwrap();
        std::fs::write(
            paths.store_dir().join("slots.json"),
            br#"[{"name":"old","id":"abc","config_dir":"/tmp/old-slot","adopted":true}]"#,
        )
        .unwrap();
        let c = Slots::open_for(&paths, "claude-code").unwrap();
        assert_eq!(c.list().len(), 1, "the pre-upgrade slot still lists");
        assert_eq!(c.get("old").unwrap().tool, "claude-code");
        let x = Slots::open_for(&paths, "codex").unwrap();
        assert!(x.list().is_empty(), "it was never a codex slot");
        // And Claude's pointer file keeps its established name, so an upgrade
        // does not silently unset the user's default account.
        c.set_default("old").unwrap();
        assert!(paths.store_dir().join("active-claude").exists());
    }

    #[test]
    fn duplicate_and_empty_names_are_rejected() {
        let root = tempfile::tempdir().unwrap();
        let paths = Paths::rooted(root.path());
        let mut s = Slots::open(&paths).unwrap();
        s.create("work").unwrap();
        assert!(s.create("work").is_err(), "duplicate name rejected");
        assert!(s.create("   ").is_err(), "empty name rejected");
    }

    #[test]
    fn id_is_stable_and_name_independent() {
        // Two slots created back-to-back get different ids (id is not the name).
        let root = tempfile::tempdir().unwrap();
        let paths = Paths::rooted(root.path());
        let mut s = Slots::open(&paths).unwrap();
        let a = s.create("alpha").unwrap();
        let b = s.create("beta").unwrap();
        assert_ne!(a.id, b.id);
        assert_ne!(a.id, "alpha", "id is opaque, not the display name");
    }

    #[test]
    fn set_default_points_at_the_slot_dir() {
        let root = tempfile::tempdir().unwrap();
        let paths = Paths::rooted(root.path());
        let mut s = Slots::open(&paths).unwrap();
        let rec = s.create("work").unwrap();
        assert_eq!(s.default_dir(), None, "no default until set");
        s.set_default("work").unwrap();
        assert_eq!(s.default_dir(), Some(rec.config_dir.clone()));
        // Re-open sees the same pointer (persisted on disk).
        assert_eq!(
            Slots::open(&paths).unwrap().default_dir(),
            Some(rec.config_dir)
        );
        assert!(s.set_default("missing").is_err(), "unknown name rejected");
    }

    #[test]
    fn rename_keeps_the_directory_so_the_login_survives() {
        let root = tempfile::tempdir().unwrap();
        let paths = Paths::rooted(root.path());
        let mut s = Slots::open(&paths).unwrap();
        let before = s.create("company2").unwrap();
        assert!(s.rename("company2", "rnd").unwrap());
        let after = s.get("rnd").expect("renamed");
        assert_eq!(
            after.config_dir, before.config_dir,
            "the directory is untouched - the Keychain item is keyed on it"
        );
        assert_eq!(after.id, before.id, "and so is the id");
        assert!(s.get("company2").is_none());
        // Colliding and missing names are refused rather than silently applied.
        s.create("other").unwrap();
        assert!(s.rename("rnd", "other").is_err(), "duplicate refused");
        assert!(
            !s.rename("   ", "x").unwrap_or(false),
            "a blank name is not a rename"
        );
        assert!(!s.rename("ghost", "x").unwrap(), "no such slot");
    }

    #[test]
    fn remove_unregisters_but_never_deletes_the_directory() {
        let root = tempfile::tempdir().unwrap();
        let paths = Paths::rooted(root.path());
        let mut s = Slots::open(&paths).unwrap();
        let rec = s.create("work").unwrap();
        s.set_default("work").unwrap();
        assert!(s.remove("work").unwrap());
        assert!(s.get("work").is_none(), "the mapping is gone");
        assert!(
            rec.config_dir.is_dir(),
            "the directory - and the login in it - is left alone"
        );
        assert_eq!(
            s.default_dir(),
            None,
            "a pointer at the removed slot is cleared, not left dangling"
        );
        // Gone means gone, and a second removal is not an error.
        assert!(!s.remove("work").unwrap());
        // It survives a reopen.
        assert!(Slots::open(&paths).unwrap().get("work").is_none());
    }

    #[test]
    fn adopt_registers_an_existing_dir_without_moving_it() {
        let root = tempfile::tempdir().unwrap();
        let paths = Paths::rooted(root.path());
        let existing = root.path().join("dot-claude-company");
        std::fs::create_dir_all(&existing).unwrap();
        let mut s = Slots::open(&paths).unwrap();
        let rec = s.adopt("company", &existing).unwrap();
        assert_eq!(rec.config_dir, existing, "config dir is the existing path");
        assert!(rec.adopted);
        assert!(existing.is_dir(), "the existing dir is left in place");
        // A non-existent dir is refused.
        assert!(s.adopt("nope", &root.path().join("absent")).is_err());
    }
}
