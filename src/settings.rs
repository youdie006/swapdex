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
    /// Seconds a turn may be HELD when every account is spent, waiting for the
    /// earliest window to reset instead of failing with a 429.
    ///
    /// The turn used to die there and an unattended run ended with it, even
    /// though the windows state their own reset times - the wall's length was
    /// known and simply not used. 0 or unset means never hold, because a caller
    /// that would rather see the error than wait must be able to.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hold_seconds: Option<i64>,
    /// Accounts kept OUT of automatic rotation. They can still be switched to by
    /// hand - this only says "do not pick this one for me", which is the useful
    /// meaning when an account is shared, billed elsewhere, or being saved.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub disabled: Vec<String>,
    /// Explicit rotation order, lowest first. Accounts absent from this list keep
    /// the automatic order and are tried after the ranked ones.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub priority: Vec<String>,
    /// Step off an account once a window reaches this fraction, instead of
    /// waiting for it to refuse a turn. `None` = wait for the refusal.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub proxy_threshold: Option<f64>,
    /// Which account to reach for when the current one is full: `roomiest` (the
    /// most left) or `consume-first` (the window about to reset, so nothing
    /// lapses unused). `None` = roomiest, the behaviour swapdex has always had.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub proxy_strategy: Option<String>,
    /// A cheaper model to ask for when EVERY account is past the threshold and
    /// there is nowhere left to rotate. Off unless set: changing the model gives
    /// the user something other than what they asked for, so it is the last
    /// thing swapdex does before a turn fails, never the first.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fallback_model: Option<String>,
}

impl Settings {
    pub fn auto(&self) -> bool {
        self.proxy_auto.unwrap_or(false)
    }

    /// Preserve the selected fraction, including thresholds below 5%. Invalid
    /// stored values are ignored instead of silently selecting another limit.
    pub fn threshold(&self) -> Option<f64> {
        self.proxy_threshold.filter(|t| valid_threshold(*t))
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
    /// The rotation strategy, defaulting to the long-standing one. An
    /// unrecognised value in the file is ignored rather than fatal: a settings
    /// file is a convenience and must never fail a command.
    pub fn strategy(&self) -> crate::proxy::pick::Strategy {
        self.proxy_strategy
            .as_deref()
            .and_then(crate::proxy::pick::Strategy::parse)
            .unwrap_or_default()
    }

    /// Carry this account's preferences to its new name. They are keyed by name
    /// and matched exactly, so a rename that skips them silently un-pauses the
    /// account and drops it out of the ranking.
    pub fn rename_account(&mut self, old: &str, new: &str) {
        for n in self.disabled.iter_mut().chain(self.priority.iter_mut()) {
            if n == old {
                *n = new.to_string();
            }
        }
    }

    /// Drop an account's preferences. Without this they outlive the account and
    /// are inherited by whatever is registered under that name next.
    pub fn forget_account(&mut self, name: &str) {
        self.disabled.retain(|n| n != name);
        self.priority.retain(|n| n != name);
    }

    pub fn rank(&self, name: &str) -> usize {
        self.priority
            .iter()
            .position(|p| p == name)
            .unwrap_or(usize::MAX)
    }
}

pub(crate) fn valid_threshold(value: f64) -> bool {
    value.is_finite() && value > 0.0 && value <= 1.0
}

/// Keep fractional percentages visible without floating-point multiplication
/// noise. Very small limits use scientific notation rather than displaying 0%.
pub(crate) fn threshold_label(value: f64) -> String {
    let percent = value * 100.0;
    let rounded = format!("{percent:.12}");
    let number = rounded.trim_end_matches('0').trim_end_matches('.');
    if number == "0" && percent > 0.0 {
        format!("{percent:e}%")
    } else {
        format!("{number}%")
    }
}

pub(crate) fn file(paths: &Paths) -> std::path::PathBuf {
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

/// Read, modify and write the settings as ONE operation.
///
/// Every caller used to do load -> modify -> save on its own, with nothing
/// between them, so two overlapping changes each read the same file and the
/// second write erased the first. teamclaude hit this as "concurrent
/// token-refresh loss": a freshly refreshed token clobbered by an unrelated
/// preference write seconds later.
///
/// The store lock serializes the whole operation. Brief contention is retried;
/// after the deadline or an I/O failure, leave settings unchanged and report it.
pub fn update(paths: &Paths, edit: impl FnOnce(&mut Settings)) -> Result<()> {
    let store = crate::store::Store::open(paths).context("open settings store")?;
    let mut attempt = 0;
    let _guard = loop {
        match store.lock() {
            Ok(guard) => break guard,
            Err(crate::store::LockError::Busy) if attempt < 49 => {
                attempt += 1;
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
            Err(crate::store::LockError::Busy) => {
                return Err(anyhow::Error::new(crate::store::LockError::Busy))
                    .context("settings stayed locked; retry after the other operation finishes");
            }
            Err(error) => {
                return Err(anyhow::Error::new(error))
                    .context("cannot lock settings; check the store permissions");
            }
        }
    };
    let mut cfg = load(paths);
    edit(&mut cfg);
    save(paths, &cfg)
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
    fn the_threshold_persists_and_invalid_stored_limits_are_ignored() {
        let root = tempfile::tempdir().unwrap();
        let paths = Paths::rooted(root.path());
        assert_eq!(load(&paths).threshold(), None, "off until asked for");
        save(
            &paths,
            &Settings {
                proxy_threshold: Some(0.9),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(load(&paths).threshold(), Some(0.9));
        // Nonsense values must not silently select a different threshold.
        let low = Settings {
            proxy_threshold: Some(0.0),
            ..Default::default()
        };
        assert_eq!(low.threshold(), None);
        let high = Settings {
            proxy_threshold: Some(5.0),
            ..Default::default()
        };
        assert_eq!(high.threshold(), None);
    }

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

#[cfg(test)]
mod concurrent_update_tests {
    use super::*;

    /// Two settings changes at once must not lose one of them.
    ///
    /// Every caller did load -> modify -> save on its own, with nothing between
    /// them. Two overlapping changes each read the same file and the second
    /// write erased the first - the pattern teamclaude hit as "concurrent
    /// token-refresh loss", where a refreshed token was clobbered by an
    /// unrelated preference write seconds later.
    #[test]
    fn overlapping_updates_both_survive() {
        let d = tempfile::tempdir().unwrap();
        let paths = crate::paths::Paths::rooted(d.path());
        save(&paths, &Settings::default()).unwrap();

        // Two writers, each changing a DIFFERENT field, interleaved the way two
        // processes would.
        let a = std::thread::spawn({
            let p = paths.clone();
            move || update(&p, |s| s.hold_seconds = Some(111)).unwrap()
        });
        let b = std::thread::spawn({
            let p = paths.clone();
            move || update(&p, |s| s.proxy_auto = Some(true)).unwrap()
        });
        a.join().unwrap();
        b.join().unwrap();

        let got = load(&paths);
        assert_eq!(got.hold_seconds, Some(111), "one writer's change was lost");
        assert_eq!(
            got.proxy_auto,
            Some(true),
            "the other writer's change was lost"
        );
    }

    #[test]
    fn renaming_an_account_carries_its_rotation_preferences() {
        let mut s = Settings {
            disabled: vec!["work".into()],
            priority: vec!["other".into(), "work".into()],
            ..Default::default()
        };
        s.rename_account("work", "work2");
        assert!(!s.is_disabled("work"), "the old name is gone");
        assert!(s.is_disabled("work2"), "the pause followed the account");
        assert_eq!(s.priority, vec!["other".to_string(), "work2".to_string()]);
    }

    #[test]
    fn forgetting_an_account_drops_its_rotation_preferences() {
        let mut s = Settings {
            disabled: vec!["work".into(), "keep".into()],
            priority: vec!["work".into(), "keep".into()],
            ..Default::default()
        };
        s.forget_account("work");
        assert!(
            !s.is_disabled("work"),
            "a later account of this name starts clean"
        );
        assert!(s.is_disabled("keep"), "other accounts are untouched");
        assert_eq!(s.priority, vec!["keep".to_string()]);
    }
}
