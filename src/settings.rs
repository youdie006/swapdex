//! Persisted preferences: `<store_dir>/settings.json`. Deliberately tiny - one
//! flat file, every field optional, an unreadable or half-written file falling
//! back to defaults rather than failing a command. Nothing here is a credential.

use crate::paths::Paths;
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct Settings {
    /// Let proxy mode continue the session on another account when one is spent.
    /// `None` = never set, treated as off; `swapdex proxy --auto` overrides it for
    /// one run.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub proxy_auto: Option<bool>,
    /// Accounts kept OUT of automatic rotation. They can still be switched to by
    /// hand - this only says "do not pick this one for me", which is the useful
    /// meaning when an account is shared, billed elsewhere, or being saved.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub disabled: Vec<String>,
    /// Explicit rotation order, lowest first. Accounts absent from this list keep
    /// the automatic order and are tried after the ranked ones.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub priority: Vec<String>,
}

impl Settings {
    pub fn auto(&self) -> bool {
        self.proxy_auto.unwrap_or(false)
    }

    pub fn is_disabled(&self, name: &str) -> bool {
        self.disabled.iter().any(|d| d == name)
    }

    /// Toggle an account's participation in rotation; returns the new state.
    pub fn toggle_disabled(&mut self, name: &str) -> bool {
        if let Some(i) = self.disabled.iter().position(|d| d == name) {
            self.disabled.remove(i);
            false
        } else {
            self.disabled.push(name.to_string());
            true
        }
    }

    /// Rank for rotation: ranked accounts first in their listed order, everything
    /// else after, so a partial ranking is still meaningful.
    pub fn rank(&self, name: &str) -> usize {
        self.priority
            .iter()
            .position(|p| p == name)
            .unwrap_or(usize::MAX)
    }
}

fn file(paths: &Paths) -> std::path::PathBuf {
    paths.store_dir().join("settings.json")
}

/// Read the settings. A missing, unreadable, or corrupt file yields defaults: a
/// preference is never worth failing a switch over.
pub fn load(paths: &Paths) -> Settings {
    std::fs::read(file(paths))
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_default()
}

/// Write the settings atomically, so a crash mid-write cannot leave a half file
/// that then reads as "no preferences".
pub fn save(paths: &Paths, s: &Settings) -> Result<()> {
    let path = file(paths);
    std::fs::create_dir_all(paths.store_dir()).context("create store dir")?;
    let bytes = serde_json::to_vec_pretty(s)?;
    // Not a secret, but reuse the atomic 0600 path: the store is 0700 anyway, and
    // this is the writer that cannot leave a half file behind.
    crate::atomic::write_secret(&path, &bytes).context("write settings.json")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn disabled_accounts_toggle_and_persist() {
        let root = tempfile::tempdir().unwrap();
        let paths = Paths::rooted(root.path());
        let mut s = load(&paths);
        assert!(!s.is_disabled("rnd"));
        assert!(s.toggle_disabled("rnd"), "first toggle disables");
        assert!(s.is_disabled("rnd"));
        save(&paths, &s).unwrap();
        let mut back = load(&paths);
        assert!(back.is_disabled("rnd"), "the choice persists");
        assert!(!back.toggle_disabled("rnd"), "toggling again re-enables");
        assert!(!back.is_disabled("rnd"));
    }

    #[test]
    fn ranked_accounts_sort_before_unranked_ones() {
        let s = Settings {
            priority: vec!["work".into(), "rnd".into()],
            ..Default::default()
        };
        assert!(s.rank("work") < s.rank("rnd"), "listed order is the order");
        assert!(
            s.rank("rnd") < s.rank("anything-else"),
            "ranked beats unranked"
        );
        assert_eq!(
            s.rank("a"),
            s.rank("b"),
            "unranked accounts keep their existing order"
        );
    }

    #[test]
    fn defaults_when_absent_and_round_trips_when_set() {
        let root = tempfile::tempdir().unwrap();
        let paths = Paths::rooted(root.path());
        assert_eq!(load(&paths), Settings::default());
        assert!(!load(&paths).auto(), "auto is off until asked for");

        save(
            &paths,
            &Settings {
                proxy_auto: Some(true),
                ..Default::default()
            },
        )
        .unwrap();
        assert!(load(&paths).auto(), "the preference persists");

        save(
            &paths,
            &Settings {
                proxy_auto: Some(false),
                ..Default::default()
            },
        )
        .unwrap();
        assert!(!load(&paths).auto(), "and can be turned back off");
    }

    #[test]
    fn a_corrupt_file_reads_as_defaults_rather_than_failing() {
        let root = tempfile::tempdir().unwrap();
        let paths = Paths::rooted(root.path());
        std::fs::create_dir_all(paths.store_dir()).unwrap();
        std::fs::write(super::file(&paths), b"{ not json").unwrap();
        assert_eq!(load(&paths), Settings::default());
    }
}
