//! Which account serves the next request. Two inputs can disagree: the user's
//! pointer (`swapdex use`, or Enter in the TUI) and the proxy's own rotation
//! after an account turned out to be spent. The rule: a pointer that CHANGED
//! since the last request is a fresh human decision and wins; otherwise a
//! rotation stands, so quota pressure does not fight the user and the user is
//! never overridden by a stale automatic choice.

use crate::slots::SlotRecord;
use std::path::{Path, PathBuf};

#[derive(Default)]
pub struct Chooser {
    last_pointer: Option<PathBuf>,
    seen_once: bool,
}

impl Chooser {
    /// The account for this request. `pointer` is the `active-claude` value,
    /// `rotated` the proxy's own current choice (Task 6).
    pub fn choose(
        &mut self,
        pointer: Option<&Path>,
        rotated: Option<&str>,
        slots: &[SlotRecord],
    ) -> Option<SlotRecord> {
        let now = pointer.map(Path::to_path_buf);
        let changed = self.seen_once && now != self.last_pointer;
        self.last_pointer = now;
        self.seen_once = true;
        let by_pointer = pointer.and_then(|p| slots.iter().find(|r| r.config_dir == p));
        if changed {
            if let Some(r) = by_pointer {
                return Some(r.clone());
            }
        }
        if let Some(name) = rotated {
            if let Some(r) = slots.iter().find(|r| r.name == name) {
                return Some(r.clone());
            }
        }
        by_pointer.or_else(|| slots.first()).cloned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn slot(name: &str, dir: &str) -> crate::slots::SlotRecord {
        crate::slots::SlotRecord {
            name: name.into(),
            id: name.into(),
            config_dir: PathBuf::from(dir),
            adopted: false,
        }
    }

    #[test]
    fn a_changed_pointer_wins_over_a_rotation() {
        let slots = vec![slot("rnd", "/s/rnd"), slot("bsgong", "/s/bsgong")];
        let mut c = Chooser::default();
        // First request follows the pointer.
        assert_eq!(
            c.choose(Some(&PathBuf::from("/s/rnd")), None, &slots)
                .unwrap()
                .name,
            "rnd"
        );
        // Quota rotated us to bsgong; the UNCHANGED pointer must not undo it.
        assert_eq!(
            c.choose(Some(&PathBuf::from("/s/rnd")), Some("bsgong"), &slots)
                .unwrap()
                .name,
            "bsgong"
        );
        // The user now points at bsgong explicitly: same account, still fine.
        assert_eq!(
            c.choose(Some(&PathBuf::from("/s/bsgong")), Some("bsgong"), &slots)
                .unwrap()
                .name,
            "bsgong"
        );
        // The user points back at rnd - a CHANGED pointer overrides the rotation.
        assert_eq!(
            c.choose(Some(&PathBuf::from("/s/rnd")), Some("bsgong"), &slots)
                .unwrap()
                .name,
            "rnd",
            "an explicit new choice overrides the rotation"
        );
    }

    #[test]
    fn unknown_pointer_falls_back_and_no_slots_yields_nothing() {
        let slots = vec![slot("rnd", "/s/rnd")];
        let mut c = Chooser::default();
        assert_eq!(
            c.choose(Some(&PathBuf::from("/nope")), None, &slots)
                .unwrap()
                .name,
            "rnd",
            "an unresolvable pointer still serves the request"
        );
        assert!(c.choose(None, None, &[]).is_none(), "no slots -> no choice");
    }

    #[test]
    fn a_rotation_naming_an_unknown_account_is_ignored() {
        let slots = vec![slot("rnd", "/s/rnd")];
        let mut c = Chooser::default();
        c.choose(Some(&PathBuf::from("/s/rnd")), None, &slots);
        assert_eq!(
            c.choose(Some(&PathBuf::from("/s/rnd")), Some("deleted"), &slots)
                .unwrap()
                .name,
            "rnd",
            "a rotation target removed meanwhile must not strand the request"
        );
    }
}
