//! Last-known usage per account, so a busy endpoint does not blank the display.
//!
//! Reading several accounts in a row rate-limits the usage endpoint, and an
//! account that could not be read this minute is not an account with no quota.
//! Keeping the last successful reading (with its age, so it is never mistaken for
//! live) means the picture degrades instead of disappearing.

use crate::paths::Paths;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct Entry {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub five_h: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub five_h_reset: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub seven_d: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub seven_d_reset: Option<i64>,
    /// Unix seconds when this reading was taken.
    pub at: i64,
}

/// A BTreeMap so the file is stable across writes - a cache that reorders itself
/// every save is noise in any diff and in any backup.
pub type Cache = BTreeMap<String, Entry>;

fn file(paths: &Paths) -> std::path::PathBuf {
    paths.store_dir().join("quota-cache.json")
}

/// Read the cache. Anything unreadable yields an empty one: a stale-value cache
/// is a convenience, never a reason to fail a command.
pub fn load(paths: &Paths) -> Cache {
    std::fs::read(file(paths))
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_default()
}

/// Merge fresh readings in and save. Only accounts that were actually read are
/// touched, so one account's throttled request cannot erase another's history.
pub fn update(paths: &Paths, fresh: &[(String, Entry)]) {
    if fresh.is_empty() {
        return;
    }
    let mut c = load(paths);
    for (name, e) in fresh {
        c.insert(name.clone(), *e);
    }
    if let Ok(bytes) = serde_json::to_vec_pretty(&c) {
        let _ = std::fs::create_dir_all(paths.store_dir());
        let _ = crate::atomic::write_secret(&file(paths), &bytes);
    }
}

/// How old a reading is, in seconds, or `None` when there is none.
pub fn age_secs(e: &Entry, now: i64) -> i64 {
    (now - e.at).max(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(pct: f64, at: i64) -> Entry {
        Entry {
            five_h: Some(pct),
            at,
            ..Default::default()
        }
    }

    #[test]
    fn readings_persist_and_merge_without_erasing_others() {
        let root = tempfile::tempdir().unwrap();
        let paths = Paths::rooted(root.path());
        assert!(load(&paths).is_empty());

        update(&paths, &[("a".into(), entry(10.0, 100))]);
        update(&paths, &[("b".into(), entry(20.0, 200))]);
        let c = load(&paths);
        assert_eq!(c.len(), 2, "a later write keeps the earlier account");
        assert_eq!(c["a"].five_h, Some(10.0));

        // A fresh reading for one account replaces only that one.
        update(&paths, &[("a".into(), entry(55.0, 300))]);
        let c = load(&paths);
        assert_eq!(c["a"].five_h, Some(55.0));
        assert_eq!(c["b"].five_h, Some(20.0), "b is untouched");
        assert_eq!(age_secs(&c["b"], 260), 60);
    }

    #[test]
    fn an_unreadable_cache_is_empty_not_an_error() {
        let root = tempfile::tempdir().unwrap();
        let paths = Paths::rooted(root.path());
        std::fs::create_dir_all(paths.store_dir()).unwrap();
        std::fs::write(super::file(&paths), b"{ not json").unwrap();
        assert!(load(&paths).is_empty());
        // And it can be written over.
        update(&paths, &[("a".into(), entry(1.0, 1))]);
        assert_eq!(load(&paths).len(), 1);
    }
}
