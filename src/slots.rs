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
}
