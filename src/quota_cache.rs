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
    /// The account keeps serving past a full window, billed to extra usage.
    /// Remembered with the numbers: without it a cached full window flips the
    /// row back to "spent" between live reads.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub on_credits: bool,
}

/// A BTreeMap so the file is stable across writes - a cache that reorders itself
/// every save is noise in any diff and in any backup.
pub type Cache = BTreeMap<String, Entry>;

fn file(paths: &Paths) -> std::path::PathBuf {
    file_for(paths, "claude-code")
}

/// One file per tool. Slot names are unique only WITHIN a tool, so a single
/// flat cache lets a Codex `work` overwrite a Claude `work` and show one
/// account's windows under the other's name. Claude keeps the original
/// filename so an upgrading install does not lose its history.
fn file_for(paths: &Paths, tool: &str) -> std::path::PathBuf {
    let name = match tool {
        "claude-code" => "quota-cache.json".to_string(),
        t => format!("{t}-quota-cache.json"),
    };
    paths.store_dir().join(name)
}

/// Read the cache. Anything unreadable yields an empty one: a stale-value cache
/// is a convenience, never a reason to fail a command.
pub fn load(paths: &Paths) -> Cache {
    load_at(paths, now_secs())
}

/// The same, for one tool.
pub fn load_for(paths: &Paths, tool: &str) -> Cache {
    load_file_at(&file_for(paths, tool), now_secs(), drops_clamped(tool))
}

/// Whether this tool's cache should discard readings pinned at the ceiling.
///
/// The rule exists for one Claude-era bug: `utilization` was read as a
/// fraction, so every account above 1% clamped to exactly 100 and the wrong
/// numbers were remembered for hours. Codex never had that bug, and applying
/// the rule there throws away the reading that matters most - a spent account's
/// - leaving its row blank instead of saying it is out.
fn drops_clamped(tool: &str) -> bool {
    tool == "claude-code"
}

/// Unix seconds, taken once per load so every window is judged against the same
/// instant.
fn now_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Drop the windows a remembered reading no longer describes.
///
/// A reading is about a WINDOW, and a window ends. Past its reset the number is
/// not merely stale, it is wrong: the account has a fresh allowance while the
/// bar goes on drawing the spent one. Found on a real machine showing "0% left"
/// ten minutes after the window had turned over. The two windows lapse
/// separately, so each is judged on its own.
fn expire_windows(mut e: Entry, now: i64) -> Entry {
    if e.five_h_reset.is_some_and(|r| now >= r) {
        e.five_h = None;
        e.five_h_reset = None;
    }
    if e.seven_d_reset.is_some_and(|r| now >= r) {
        e.seven_d = None;
        e.seven_d_reset = None;
    }
    e
}

/// `load`, against a given instant, so the expiry is testable.
fn load_at(paths: &Paths, now: i64) -> Cache {
    load_file_at(&file(paths), now, true)
}

fn load_file_at(path: &std::path::Path, now: i64, drop_clamped: bool) -> Cache {
    let mut c: Cache = std::fs::read(path)
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_default();
    for e in c.values_mut() {
        *e = expire_windows(*e, now);
    }
    // An entry with nothing left to say is not an entry.
    c.retain(|_, e| e.five_h.is_some() || e.seven_d.is_some());
    // Readings taken while `utilization` was misread as a fraction are all
    // exactly 100 - every account above 1% clamped there - and remembering them
    // would keep showing accounts as spent long after the reading was fixed.
    // A genuine 100 is re-read within minutes, so dropping it costs nothing.
    if drop_clamped {
        c.retain(|_, e| !was_clamped(e));
    }
    c
}

/// A reading that carries only the clamp value in both windows: not a
/// measurement, an artefact of the misread.
fn was_clamped(e: &Entry) -> bool {
    let at_ceiling = |v: Option<f64>| v.is_none_or(|p| p >= 100.0);
    at_ceiling(e.five_h) && at_ceiling(e.seven_d) && (e.five_h.is_some() || e.seven_d.is_some())
}

/// Merge fresh readings in and save. Only accounts that were actually read are
/// touched, so one account's throttled request cannot erase another's history.
pub fn update(paths: &Paths, fresh: &[(String, Entry)]) {
    update_for(paths, "claude-code", fresh);
}

/// The same, for one tool.
pub fn update_for(paths: &Paths, tool: &str, fresh: &[(String, Entry)]) {
    if fresh.is_empty() {
        return;
    }
    let path = file_for(paths, tool);
    let mut c = load_file_at(&path, now_secs(), drops_clamped(tool));
    for (name, e) in fresh {
        c.insert(name.clone(), *e);
    }
    if let Ok(bytes) = serde_json::to_vec_pretty(&c) {
        let _ = std::fs::create_dir_all(paths.store_dir());
        let _ = crate::atomic::write_secret(&path, &bytes);
    }
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

    /// The clamp rule belongs to Claude, where the misread happened. Applied to
    /// Codex it discards a genuinely spent account's reading and leaves its row
    /// blank - the one row you most need to see.
    #[test]
    fn a_spent_codex_account_is_still_remembered() {
        let root = tempfile::tempdir().unwrap();
        let paths = Paths::rooted(root.path());
        let full = Entry {
            seven_d: Some(100.0),
            at: 1,
            ..Default::default()
        };
        update_for(&paths, "codex", &[("spent".into(), full)]);
        update_for(&paths, "claude-code", &[("spent".into(), full)]);

        assert_eq!(load_for(&paths, "codex")["spent"].seven_d, Some(100.0));
        // Claude keeps the rule it needs, unchanged.
        assert!(!load_for(&paths, "claude-code").contains_key("spent"));
    }

    /// Codex accounts are remembered in their own file. Slot names are only
    /// unique within a tool, so one flat cache lets a Codex account named
    /// `work` overwrite a Claude account named `work` - and the display would
    /// show one account's windows under the other's name.
    #[test]
    fn each_tool_remembers_its_accounts_separately() {
        let root = tempfile::tempdir().unwrap();
        let paths = Paths::rooted(root.path());
        update_for(&paths, "claude-code", &[("work".into(), entry(10.0, 1))]);
        update_for(&paths, "codex", &[("work".into(), entry(90.0, 1))]);

        assert_eq!(load_for(&paths, "claude-code")["work"].five_h, Some(10.0));
        assert_eq!(load_for(&paths, "codex")["work"].five_h, Some(90.0));
        // The Claude file keeps the name it has always had, so an install that
        // upgrades does not lose its history.
        assert!(root
            .path()
            .join(".local/share/swapdex/quota-cache.json")
            .exists());
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
    }

    // Every reading taken while utilization was misread is exactly 100, and
    // keeping them would show accounts as spent long after the reading was fixed.
    #[test]
    fn readings_pinned_at_the_clamp_are_not_remembered() {
        let root = tempfile::tempdir().unwrap();
        let paths = Paths::rooted(root.path());
        let full = Entry {
            five_h: Some(100.0),
            seven_d: Some(100.0),
            at: 1,
            ..Default::default()
        };
        let real = Entry {
            five_h: Some(4.0),
            seven_d: Some(59.0),
            at: 2,
            ..Default::default()
        };
        // A real 100% in ONE window beside a measured other window is a genuine
        // reading and must survive - only the all-at-the-ceiling shape is dropped.
        let one_full = Entry {
            five_h: Some(100.0),
            seven_d: Some(59.0),
            at: 3,
            ..Default::default()
        };
        update(
            &paths,
            &[
                ("clamped".into(), full),
                ("measured".into(), real),
                ("half".into(), one_full),
            ],
        );
        let c = load(&paths);
        assert!(!c.contains_key("clamped"), "the artefact is dropped");
        assert_eq!(c["measured"].five_h, Some(4.0));
        assert_eq!(c["half"].five_h, Some(100.0), "a real 100 survives");
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

#[cfg(test)]
mod expiry_tests {
    use super::*;

    /// A remembered reading describes a WINDOW, and a window ends. Once its reset
    /// has passed the number is not stale, it is wrong: the account has a fresh
    /// allowance and the bar was still drawing the spent one. Found on a real
    /// machine reading "0% left" ten minutes after the window had turned over.
    #[test]
    fn a_window_whose_reset_has_passed_is_dropped_not_shown() {
        let e = Entry {
            five_h: Some(100.0),
            five_h_reset: Some(1_000),
            seven_d: Some(41.0),
            seven_d_reset: Some(9_000),
            at: 900,
            ..Default::default()
        };
        let before = expire_windows(e, 999);
        assert_eq!(before.five_h, Some(100.0), "inside the window it stands");

        let after = expire_windows(e, 1_001);
        assert_eq!(after.five_h, None, "past the reset it is gone");
        assert_eq!(after.five_h_reset, None, "and so is the reset it described");
        assert_eq!(
            after.seven_d,
            Some(41.0),
            "the other window is untouched - they turn over separately"
        );
    }

    /// A reading with no reset time says nothing about when it lapses, so it is
    /// left alone; its age is already shown beside it.
    #[test]
    fn a_reading_with_no_reset_is_left_alone() {
        let e = Entry {
            five_h: Some(30.0),
            five_h_reset: None,
            seven_d: None,
            seven_d_reset: None,
            at: 900,
            ..Default::default()
        };
        assert_eq!(expire_windows(e, 9_999_999), e);
    }

    #[test]
    fn an_entry_left_with_nothing_is_not_kept() {
        let root = tempfile::tempdir().unwrap();
        let paths = Paths::rooted(root.path());
        update(
            &paths,
            &[(
                "spent".to_string(),
                // Not 100: a reading pinned at the ceiling in every window is
                // dropped as an artefact of the old misread, which would hide
                // the behaviour under test.
                Entry {
                    five_h: Some(80.0),
                    five_h_reset: Some(1_000),
                    at: 900,
                    ..Default::default()
                },
            )],
        );
        assert!(
            !load_at(&paths, 2_000).contains_key("spent"),
            "nothing left to say about it"
        );
        assert!(
            load_at(&paths, 999).contains_key("spent"),
            "and it was there while the window ran"
        );
    }
}
