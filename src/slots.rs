//! The permanent-slot registry: a name -> slot mapping persisted to
//! `<store_dir>/slots.json`. Each slot is a directory under
//! `<store_dir>/slots/<id>/` used as a Claude `CLAUDE_CONFIG_DIR`. swapdex never
//! writes a credential into a slot; the tool's own login does. The id is opaque
//! and name-independent so a rename never changes the directory (and therefore
//! never changes the Keychain service, which is derived from the dir string).

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
}

pub struct Slots {
    file: PathBuf,
    slots_dir: PathBuf,
    records: Vec<SlotRecord>,
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
    pub fn open(paths: &Paths) -> Result<Slots> {
        let store = paths.store_dir();
        let file = store.join("slots.json");
        let records = if file.exists() {
            let bytes = std::fs::read(&file).context("read slots.json")?;
            serde_json::from_slice(&bytes).context("slots.json is corrupt")?
        } else {
            Vec::new()
        };
        Ok(Slots {
            file,
            slots_dir: store.join("slots"),
            records,
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
        };
        self.records.push(rec.clone());
        self.persist()?;
        Ok(rec)
    }

    fn persist(&self) -> Result<()> {
        if let Some(parent) = self.file.parent() {
            std::fs::create_dir_all(parent).context("create store dir")?;
        }
        let bytes = serde_json::to_vec_pretty(&self.records)?;
        std::fs::write(&self.file, bytes).context("write slots.json")?;
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
        };
        self.records.push(rec.clone());
        self.persist()?;
        Ok(rec)
    }

    /// The pointer file the `claude` shim reads to find the default account's
    /// slot: `<store_dir>/active-claude`.
    fn pointer_file(&self) -> PathBuf {
        // `self.file` is `<store_dir>/slots.json`; its parent is the store dir.
        let store = self
            .file
            .parent()
            .map(|p| p.to_path_buf())
            .unwrap_or_default();
        store.join("active-claude")
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
            .context("write active-claude pointer")?;
        Ok(())
    }

    /// The default account's slot dir, if a default has been set.
    pub fn default_dir(&self) -> Option<PathBuf> {
        let s = std::fs::read_to_string(self.pointer_file()).ok()?;
        let s = s.trim();
        (!s.is_empty()).then(|| PathBuf::from(s))
    }
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
