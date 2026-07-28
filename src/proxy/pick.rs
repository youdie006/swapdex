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

/// Is this account too full to start a turn on? A window at or past `threshold`
/// (a fraction, so 0.98 is 98%) is treated as gone: the next turn would very
/// likely be the one that hits the wall, and stepping across BEFORE that keeps a
/// conversation from ever seeing a refusal.
///
/// `None` for either window means "not measured", never "empty" - an unmeasured
/// account must not be skipped on a guess.
pub fn over_threshold(five_h: Option<f64>, seven_d: Option<f64>, threshold: f64) -> bool {
    let limit = (threshold * 100.0).clamp(0.0, 100.0);
    [five_h, seven_d].into_iter().flatten().any(|p| p >= limit)
}

/// How much room an account has left: the worst of its measured windows, so an
/// account with a spent 7d is not called roomy because its 5h happens to be
/// fresh. `None` when nothing about it has been measured.
pub fn headroom(five_h: Option<f64>, seven_d: Option<f64>) -> Option<f64> {
    let worst = [five_h, seven_d]
        .into_iter()
        .flatten()
        .fold(f64::NAN, f64::max);
    worst.is_finite().then(|| (100.0 - worst).clamp(0.0, 100.0))
}

/// Order candidates by room left, most first. An explicit rank still wins - the
/// user saying "prefer this one" outranks a percentage - and unmeasured accounts
/// come after measured ones rather than being assumed empty or assumed free.
pub fn by_headroom<'a, T>(
    items: &mut [T],
    rank: impl Fn(&T) -> usize,
    room: impl Fn(&T) -> Option<f64>,
) where
    T: 'a,
{
    items.sort_by(|a, b| {
        rank(a).cmp(&rank(b)).then_with(|| {
            match (room(a), room(b)) {
                (Some(x), Some(y)) => y.total_cmp(&x), // more room first
                (Some(_), None) => std::cmp::Ordering::Less,
                (None, Some(_)) => std::cmp::Ordering::Greater,
                (None, None) => std::cmp::Ordering::Equal,
            }
        })
    });
}

/// The next account to try after `current` proved spent: the first slot that is
/// neither `current` nor known-spent. `None` when nothing is left, which the
/// caller reports rather than silently retrying a dead account.
///
/// (Under sustained load "soonest reset first" is the better rule; first-eligible
/// is enough while accounts are spent one at a time, and it needs no reset clock.)
pub fn rotate_target(
    current: &str,
    slots: &[SlotRecord],
    state: &std::collections::HashMap<String, crate::proxy::ratelimit::Quota>,
) -> Option<String> {
    slots
        .iter()
        .filter(|r| r.name != current)
        .find(|r| !state.get(&r.name).is_some_and(|q| q.rejected))
        .map(|r| r.name.clone())
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
    fn rotation_skips_the_current_and_the_known_spent_accounts() {
        use crate::proxy::ratelimit::Quota;
        let slots = vec![
            slot("rnd", "/s/rnd"),
            slot("bsgong", "/s/b"),
            slot("claude", "/s/c"),
        ];
        let spent = |name: &str| {
            (
                name.to_string(),
                Quota {
                    rejected: true,
                    ..Default::default()
                },
            )
        };
        let mut state: std::collections::HashMap<String, Quota> =
            [spent("rnd"), spent("bsgong")].into_iter().collect();
        assert_eq!(
            rotate_target("rnd", &slots, &state).as_deref(),
            Some("claude"),
            "the first account that is neither current nor spent"
        );
        state.extend([spent("claude")]);
        assert_eq!(
            rotate_target("rnd", &slots, &state),
            None,
            "every account spent -> nothing to rotate to"
        );
    }

    #[test]
    fn headroom_is_the_worst_window_not_the_best() {
        // A spent weekly window means little room, however fresh the 5h is.
        assert_eq!(headroom(Some(2.0), Some(97.0)), Some(3.0));
        assert_eq!(headroom(Some(40.0), None), Some(60.0));
        assert_eq!(headroom(None, None), None, "unmeasured is not empty");
    }

    #[test]
    fn candidates_sort_by_room_with_explicit_rank_winning() {
        // (rank, headroom)
        let mut v = vec![
            ("plenty", usize::MAX, Some(90.0)),
            ("scarce", usize::MAX, Some(5.0)),
            ("unknown", usize::MAX, None),
            ("pinned", 0, Some(1.0)),
        ];
        by_headroom(&mut v, |t| t.1, |t| t.2);
        assert_eq!(
            v.iter().map(|t| t.0).collect::<Vec<_>>(),
            vec!["pinned", "plenty", "scarce", "unknown"],
            "a pinned account first, then most room, unmeasured last"
        );
    }

    #[test]
    fn over_threshold_only_fires_on_a_measured_window() {
        // 98%: at or past it counts as gone.
        assert!(over_threshold(Some(98.0), None, 0.98));
        assert!(over_threshold(Some(99.5), None, 0.98));
        assert!(
            over_threshold(None, Some(100.0), 0.98),
            "either window can trip it"
        );
        assert!(!over_threshold(Some(97.9), Some(50.0), 0.98));
        // Unmeasured is not empty: an account with no reading is left alone.
        assert!(!over_threshold(None, None, 0.98));
        // A threshold of 1.0 means "only when actually full".
        assert!(!over_threshold(Some(99.0), None, 1.0));
        assert!(over_threshold(Some(100.0), None, 1.0));
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
