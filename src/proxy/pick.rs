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

/// The one-line usage summary: what each account reads, and for the ones that
/// read nothing, why.
///
/// `measured` is (name, rendered value) for accounts with a number - including
/// values carried over from an earlier round. `unread` is (name, reason) for
/// this round's failures. An account in BOTH keeps its number: a failed re-read
/// does not erase what was already known, and printing both made one account
/// appear twice on a line that then contradicted itself.
/// The same information as `usage_line`, one account per line.
///
/// With both windows and both reset times, a single joined line ran past 150
/// characters and stopped being readable - which defeats the point of printing
/// it. `swapdex quota` already lays accounts out this way; the log follows.
pub fn usage_block(
    measured: &[(String, String)],
    unread: &[(String, String)],
    refused: &[String],
) -> Vec<String> {
    let width = measured
        .iter()
        .map(|(n, _)| n.chars().count())
        .chain(unread.iter().map(|(n, _)| n.chars().count()))
        .max()
        .unwrap_or(0);
    let mut out: Vec<String> = measured
        .iter()
        .map(|(n, v)| {
            let mark = if refused.iter().any(|r| r == n) {
                "  - refusing turns"
            } else {
                ""
            };
            format!("{n:width$}  {v}{mark}")
        })
        .collect();
    out.sort();
    let mut rest: Vec<String> = unread
        .iter()
        .filter(|(n, _)| !measured.iter().any(|(m, _)| m == n))
        .map(|(n, why)| format!("{n:width$}  ({why})"))
        .collect();
    rest.sort();
    out.extend(rest);
    out
}

pub fn usage_line(
    measured: &[(String, String)],
    unread: &[(String, String)],
    refused: &[String],
) -> String {
    // A percentage is what an account's WINDOWS say; whether it will actually
    // take a turn is what its last answer said. Those can disagree - an account
    // reading 0% whose overage is spent refuses every turn - and printing only
    // the percentage promises a reserve that is not there.
    let mut parts: Vec<String> = measured
        .iter()
        .map(|(n, v)| {
            if refused.iter().any(|r| r == n) {
                format!("{n} {v} but refusing turns")
            } else {
                format!("{n} {v}")
            }
        })
        .collect();
    parts.sort();
    let mut rest: Vec<String> = unread
        .iter()
        .filter(|(n, _)| !measured.iter().any(|(m, _)| m == n))
        .map(|(n, why)| format!("{n} ({why})"))
        .collect();
    rest.sort();
    parts.extend(rest);
    parts.join(", ")
}

/// Is this redirection worth saying out loud again?
///
/// The account a `serve` pointer names can be benched, and then every turn is
/// redirected to the same place - so the same sentence printed on every turn
/// for as long as the bench lasted. A line repeated that often stops being read,
/// and it buries the ones that matter.
///
/// It is said once per episode: when the pair changes, or after a turn that was
/// not redirected at all (the situation ended and has now come back). `memo`
/// carries that state between turns.
pub fn announce_bench(memo: &mut Option<(String, String)>, from: &str, to: &str) -> bool {
    let pair = (from.to_string(), to.to_string());
    if memo.as_ref() == Some(&pair) {
        return false;
    }
    *memo = Some(pair);
    true
}

/// A turn served without redirection: the episode is over, so the next one is
/// worth announcing again.
pub fn clear_bench_note(memo: &mut Option<(String, String)>) {
    *memo = None;
}

/// One window as it should READ: how much is left, said in words.
///
/// swapdex speaks one language about quota - what is LEFT. The dashboard gauge
/// says `62% left` and `swapdex quota` says `39% left`, but this line printed
/// the percentage USED with no word, so `5h 0%` meant a full window while
/// looking exactly like an empty one. One tool cannot hold two conventions and
/// expect either to be trusted.
///
/// `used_pct` is what the endpoint reports; the conversion happens here, at the
/// edge, so every decision inside still reasons about usage.
pub fn window_left(label: &str, used_pct: f64, resets: Option<String>) -> String {
    let left = (100.0 - used_pct).clamp(0.0, 100.0);
    match resets {
        Some(r) => format!("{label} {left:.0}% left, resets {r}"),
        None => format!("{label} {left:.0}% left"),
    }
}

/// A reset time as a CLOCK, for output that is written once and read later.
///
/// The dashboard counts down because it redraws; a log line does not. A
/// countdown printed into a file is right for one second and overstates the
/// wait by however long ago it was written, so what goes here is the time
/// itself. Same-day stays bare (`3pm`); anything further carries its day, which
/// is the one convention every tool surveyed arrived at independently.
///
/// `at` and `now` are unix seconds; `tz_offset` is the local offset in seconds.
pub fn reset_clock(at: i64, now: i64, tz_offset: i64) -> String {
    if at <= now {
        return "now".into();
    }
    let l = at + tz_offset;
    let (h24, mins) = ((l % 86400) / 3600, (l % 3600) / 60);
    let ampm = if h24 < 12 { "am" } else { "pm" };
    let h12 = match h24 % 12 {
        0 => 12,
        h => h,
    };
    // Minutes are dropped on the hour, the way a person says it.
    let time = if mins == 0 {
        format!("{h12}{ampm}")
    } else {
        format!("{h12}:{mins:02}{ampm}")
    };
    if at - now < 86400 && (l / 86400) == ((now + tz_offset) / 86400) {
        return time;
    }
    const DAY: [&str; 7] = ["Thu", "Fri", "Sat", "Sun", "Mon", "Tue", "Wed"];
    format!("{} {time}", DAY[((l / 86400) % 7) as usize])
}

/// The account identities already ruled out this turn.
///
/// Everything else here keys off a slot NAME, but a rate limit belongs to the
/// ACCOUNT - and one account can sit in two slots. 병승's Mac has exactly that:
/// `~/.claude` and `~/.claude-company` are both `8dd1a9aa-...`. Handing the next
/// turn from one to the other looks like a rotation and buys nothing, because
/// the wall it just hit is the same wall.
///
/// `named` is (slot name, that slot's account uuid). A slot whose uuid cannot be
/// read contributes nothing: unknown is not the same as spent, and excluding on
/// a blank would rule out every account at once.
pub fn burned_uuids(named: &[(String, Option<String>)], ruled_out: &[String]) -> Vec<String> {
    let mut out: Vec<String> = named
        .iter()
        .filter(|(name, _)| ruled_out.iter().any(|r| r == name))
        .filter_map(|(_, uuid)| uuid.clone())
        .collect();
    out.sort();
    out.dedup();
    out
}

/// One candidate, reduced to the facts the choice actually turns on.
#[derive(Clone, Debug, PartialEq)]
pub struct Candidate {
    pub name: String,
    /// The account this slot holds, when it can be read.
    pub uuid: Option<String>,
    /// Ruled out by name already: tried this turn, known spent, benched, or
    /// disabled by the user.
    pub ruled_out: bool,
    /// Has a login that could serve.
    pub usable: bool,
}

/// The next slot that can serve, with the identity rule applied.
///
/// Pure so the identity rule is actually exercised. The version of this that
/// lived inline was covered only by a test of its helper, and removing the rule
/// from the caller broke nothing - the same way a fix shipped three times in one
/// day changed nothing anyone could see.
pub fn next_usable(candidates: &[Candidate]) -> Option<&Candidate> {
    let ruled_out: Vec<String> = candidates
        .iter()
        .filter(|c| c.ruled_out)
        .map(|c| c.name.clone())
        .collect();
    let named: Vec<(String, Option<String>)> = candidates
        .iter()
        .map(|c| (c.name.clone(), c.uuid.clone()))
        .collect();
    let burned = burned_uuids(&named, &ruled_out);
    candidates.iter().find(|c| {
        !c.ruled_out
            && c.usable
            && !c
                .uuid
                .as_deref()
                .is_some_and(|u| burned.iter().any(|b| b == u))
    })
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

/// The same, told whether the account can keep serving past its windows.
///
/// Credits do NOT keep a full account in front. They cost money, and the whole
/// point of stepping off at a threshold is to reach for an account that still
/// has free room. Blocking the step meant swapdex burned credits on a capped
/// account while another sat idle with quota to spare - so the flag no longer
/// suppresses the threshold.
///
/// Where credits DO matter is the fallback: when nothing else is below the
/// threshold, the proxy stays put, and an account with credits can still serve
/// that turn. That path needs no flag - it is what "no better account, stay
/// here" already does.
pub fn over_threshold_with(
    five_h: Option<f64>,
    seven_d: Option<f64>,
    threshold: f64,
    _credits_available: bool,
) -> bool {
    over_threshold(five_h, seven_d, threshold)
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
mod usage_block_tests {
    use super::*;

    fn p(v: &[(&str, &str)]) -> Vec<(String, String)> {
        v.iter()
            .map(|(a, b)| (a.to_string(), b.to_string()))
            .collect()
    }

    /// One account per line, names aligned, and the same rules the single line
    /// had: a number survives a failed re-read, and nothing is named twice.
    #[test]
    fn each_account_gets_its_own_line() {
        let out = usage_block(
            &p(&[
                ("bsgong", "5h 61% resets 1:47pm · 7d 27% resets Tue 9am"),
                ("rnd", "5h 0% · 7d 6% resets Sun 5pm"),
            ]),
            &p(&[("personal", "no saved token"), ("bsgong", "throttled")]),
            &[],
        );
        assert_eq!(out.len(), 3, "one line each, bsgong not repeated: {out:?}");
        assert!(out[0].starts_with("bsgong"), "{out:?}");
        assert!(
            out.iter().any(|l| l.contains("(no saved token)")),
            "{out:?}"
        );
        // Names line up, so the values form a column. Measured from where the
        // VALUE starts, not from the first double space - that one lands right
        // after each name and differs by name length even when the layout is
        // perfect, which is what the first version of this check got wrong.
        let starts: Vec<usize> = out
            .iter()
            .map(|l| {
                let name_end = l.find(' ').expect("name then padding");
                name_end + (l[name_end..].len() - l[name_end..].trim_start().len())
            })
            .collect();
        assert!(
            starts.windows(2).all(|w| w[0] == w[1]),
            "values form a column: {starts:?} in {out:?}"
        );
    }

    #[test]
    fn an_account_refusing_turns_is_marked() {
        let out = usage_block(&p(&[("rnd", "5h 0%")]), &[], &["rnd".to_string()]);
        assert!(out[0].contains("refusing"), "{out:?}");
    }
}

#[cfg(test)]
mod announce_bench_tests {
    use super::*;

    /// A `serve` pointer naming a benched account redirects every turn, and the
    /// same sentence printed on every one of them. Said once instead.
    #[test]
    fn the_same_redirection_is_announced_once() {
        let mut memo = None;
        assert!(
            announce_bench(&mut memo, "rnd", "bsgong"),
            "first time, say it"
        );
        assert!(
            !announce_bench(&mut memo, "rnd", "bsgong"),
            "same again, stay quiet"
        );
        assert!(!announce_bench(&mut memo, "rnd", "bsgong"));
    }

    /// A different destination is a different fact.
    #[test]
    fn a_new_destination_is_announced() {
        let mut memo = None;
        announce_bench(&mut memo, "rnd", "bsgong");
        assert!(announce_bench(&mut memo, "rnd", "personal"));
        assert!(announce_bench(&mut memo, "personal", "bsgong"));
    }

    /// Once a turn goes through without redirection the episode is over, so its
    /// return is news again - otherwise a bench that came and went would be
    /// announced only the very first time it ever happened.
    #[test]
    fn the_episode_ending_makes_the_next_one_news() {
        let mut memo = None;
        announce_bench(&mut memo, "rnd", "bsgong");
        clear_bench_note(&mut memo);
        assert!(announce_bench(&mut memo, "rnd", "bsgong"));
    }
}

#[cfg(test)]
mod window_left_tests {
    use super::*;

    /// The line printed the percentage USED with no word while every other
    /// surface said what was LEFT, so "5h 0%" meant a full window and read as
    /// an empty one. 병승 read it exactly that way.
    #[test]
    fn a_window_reports_what_is_left_not_what_is_used() {
        assert_eq!(window_left("5h", 0.0, None), "5h 100% left");
        assert_eq!(window_left("7d", 30.0, None), "7d 70% left");
        assert_eq!(window_left("5h", 100.0, None), "5h 0% left");
    }

    #[test]
    fn the_reset_rides_along_when_there_is_one() {
        let s = window_left("5h", 61.0, Some("1:47pm".into()));
        assert_eq!(s, "5h 39% left, resets 1:47pm");
    }

    /// A reading outside 0..100 (a rounding artefact from the endpoint) must not
    /// print a negative allowance.
    #[test]
    fn a_reading_past_the_ends_is_clamped() {
        assert_eq!(window_left("5h", 105.0, None), "5h 0% left");
        assert_eq!(window_left("5h", -3.0, None), "5h 100% left");
    }
}

#[cfg(test)]
mod reset_clock_tests {
    use super::*;

    /// A log line is read after it is written, so it carries the time rather
    /// than a countdown that decays into a lie the moment it is printed.
    #[test]
    fn a_same_day_reset_is_just_the_time() {
        // 1970-01-01 00:00 UTC + 15h = 3pm, still the same day.
        assert_eq!(reset_clock(15 * 3600, 9 * 3600, 0), "3pm");
        assert_eq!(reset_clock(15 * 3600 + 30 * 60, 9 * 3600, 0), "3:30pm");
        assert_eq!(reset_clock(9 * 3600, 8 * 3600, 0), "9am");
    }

    /// Past due says so rather than printing a time that already went by.
    #[test]
    fn a_reset_already_past_reads_as_now() {
        assert_eq!(reset_clock(100, 100, 0), "now");
        assert_eq!(reset_clock(50, 100, 0), "now");
    }

    /// Beyond today it carries the day - the one convention every surveyed tool
    /// reinvented, because "3pm" three days out is a trap.
    #[test]
    fn a_reset_on_another_day_carries_the_day() {
        let out = reset_clock(3 * 86400 + 15 * 3600, 9 * 3600, 0);
        assert!(out.contains("3pm"), "{out}");
        assert!(out.len() > "3pm".len(), "it names the day too: {out}");
    }
}

#[cfg(test)]
mod usage_line_tests {
    use super::*;

    fn p(v: &[(&str, &str)]) -> Vec<(String, String)> {
        v.iter()
            .map(|(a, b)| (a.to_string(), b.to_string()))
            .collect()
    }

    /// The finding that prompted this: rnd measured 0% and was printed as a
    /// fresh reserve, two lines below its own answers saying its overage was
    /// spent. The threshold handed it the session, it refused twice, and the
    /// session came straight back - while the line still said 0%.
    #[test]
    fn an_account_that_is_refusing_turns_does_not_read_as_a_reserve() {
        let line = usage_line(&p(&[("rnd", "0% (on credits)")]), &[], &["rnd".to_string()]);
        assert!(line.contains("refusing"), "{line}");
        assert!(
            line.contains("0%"),
            "the measurement is still shown: {line}"
        );
    }

    /// The line that gave this away: bsgong had a number carried over AND a
    /// throttled re-read this round, so it was printed twice - a single line
    /// claiming an account was both 89% and unreadable.
    #[test]
    fn an_account_with_a_number_is_never_also_listed_as_unread() {
        let line = usage_line(
            &p(&[("bsgong", "89% (on credits)")]),
            &p(&[
                ("bsgong", "usage endpoint throttled"),
                ("rnd", "usage endpoint throttled"),
            ]),
            &[],
        );
        assert_eq!(
            line,
            "bsgong 89% (on credits), rnd (usage endpoint throttled)"
        );
        assert_eq!(line.matches("bsgong").count(), 1, "named once: {line}");
    }

    /// A failed re-read must not erase a number that was already known - that is
    /// what makes the threshold keep working across a throttled round.
    #[test]
    fn a_failed_reread_keeps_the_number_it_already_had() {
        let line = usage_line(
            &p(&[("a", "50%")]),
            &p(&[("a", "usage endpoint throttled")]),
            &[],
        );
        assert_eq!(line, "a 50%");
    }

    #[test]
    fn accounts_with_no_number_are_named_with_their_reason() {
        let line = usage_line(
            &p(&[]),
            &p(&[("b", "no saved token"), ("a", "throttled")]),
            &[],
        );
        assert_eq!(line, "a (throttled), b (no saved token)");
    }
}

#[cfg(test)]
mod identity_tests {
    use super::*;

    fn named() -> Vec<(String, Option<String>)> {
        vec![
            ("bsgong".into(), Some("8dd1a9aa".into())),
            ("bsgong-slot".into(), Some("8dd1a9aa".into())),
            ("rnd".into(), Some("202743db".into())),
        ]
    }

    /// The real shape of 병승's Mac: two slots, one account. Once one has hit the
    /// wall, the other is behind the same wall.
    #[test]
    fn one_account_in_two_slots_is_burned_once_either_is() {
        let burned = burned_uuids(&named(), &["bsgong".to_string()]);
        assert_eq!(burned, vec!["8dd1a9aa"], "its twin is ruled out too");
        assert!(
            !burned.contains(&"202743db".to_string()),
            "a genuinely different account is untouched"
        );
    }

    /// A uuid that cannot be read must not rule anything out - excluding on a
    /// blank would burn every account whose identity is merely unknown.
    #[test]
    fn an_unreadable_identity_rules_nothing_out() {
        let named = vec![("a".to_string(), None), ("b".to_string(), None)];
        assert!(burned_uuids(&named, &["a".to_string()]).is_empty());
    }

    fn c(name: &str, uuid: Option<&str>, ruled_out: bool) -> Candidate {
        Candidate {
            name: name.into(),
            uuid: uuid.map(str::to_string),
            ruled_out,
            usable: true,
        }
    }

    /// The whole point, end to end: bsgong just hit the wall, and bsgong-slot is
    /// the SAME login in another directory. The turn must go to rnd.
    #[test]
    fn a_spent_account_does_not_hand_the_turn_to_its_own_twin() {
        let list = [
            c("bsgong", Some("8dd1a9aa"), true),
            c("bsgong-slot", Some("8dd1a9aa"), false),
            c("rnd", Some("202743db"), false),
        ];
        assert_eq!(
            next_usable(&list).map(|c| c.name.as_str()),
            Some("rnd"),
            "the twin is behind the same wall"
        );
    }

    /// With no twin in the list, nothing changes about the ordinary case.
    #[test]
    fn an_ordinary_failover_still_moves_to_the_next_account() {
        let list = [c("a", Some("u-a"), true), c("b", Some("u-b"), false)];
        assert_eq!(next_usable(&list).map(|c| c.name.as_str()), Some("b"));
    }

    /// Identity unknown must not exclude: on a store where no uuid can be read,
    /// the rule would otherwise rule out every remaining account at once.
    #[test]
    fn unknown_identities_do_not_block_a_failover() {
        let list = [c("a", None, true), c("b", None, false)];
        assert_eq!(next_usable(&list).map(|c| c.name.as_str()), Some("b"));
    }

    #[test]
    fn a_slot_with_no_usable_login_is_never_offered() {
        let mut only = c("a", Some("u"), false);
        only.usable = false;
        assert_eq!(next_usable(&[only]), None);
    }

    #[test]
    fn nothing_tried_burns_nothing() {
        assert!(burned_uuids(&named(), &[]).is_empty());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn slot(name: &str, dir: &str) -> crate::slots::SlotRecord {
        crate::slots::SlotRecord {
            tool: "claude-code".into(),
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

/// Accounts held out of the rotation after a refusal, and for how long.
///
/// A 401 used to sideline an account for the life of the proxy: two inserts, no
/// removal anywhere. The remedy the proxy itself prints is "sign it in again" -
/// and after doing exactly that the account was still skipped, silently, until
/// the user found and killed a background process they were never told about.
///
/// So the exclusion expires. A dead login costs one failed request per window to
/// re-confirm, which is the price of not stranding an account that has been
/// fixed. An explicit `clear` re-admits one immediately, for when the user says
/// which account they want.
#[derive(Default)]
pub struct Sidelined {
    marks: std::collections::HashMap<String, std::time::Instant>,
}

/// How long a refusal keeps an account out. Long enough that a genuinely dead
/// login is not retried on every turn, short enough that a sign-in taken in the
/// meantime is noticed without anyone restarting anything.
pub const SIDELINE_FOR: std::time::Duration = std::time::Duration::from_secs(600);

impl Sidelined {
    pub fn mark(&mut self, name: &str, now: std::time::Instant) {
        self.marks.insert(name.to_string(), now);
    }

    pub fn contains(&self, name: &str, now: std::time::Instant) -> bool {
        self.marks
            .get(name)
            .is_some_and(|at| now.duration_since(*at) < SIDELINE_FOR)
    }

    /// Put one back in the rotation now - the user named it.
    pub fn clear(&mut self, name: &str) {
        self.marks.remove(name);
    }

    /// How many are currently held out, for "everything is sidelined" checks.
    pub fn active(&self, now: std::time::Instant) -> usize {
        self.marks
            .values()
            .filter(|at| now.duration_since(**at) < SIDELINE_FOR)
            .count()
    }
}

#[cfg(test)]
mod sidelined_tests {
    use super::*;
    use std::time::Instant;

    #[test]
    fn a_refusal_holds_an_account_out_then_lets_it_back() {
        let mut s = Sidelined::default();
        let t0 = Instant::now();
        s.mark("rnd", t0);
        assert!(s.contains("rnd", t0), "held out right after the refusal");
        assert!(
            s.contains("rnd", t0 + SIDELINE_FOR - std::time::Duration::from_secs(1)),
            "still held out inside the window"
        );
        assert!(
            !s.contains("rnd", t0 + SIDELINE_FOR),
            "and offered again once it lapses - a login fixed meanwhile must be usable"
        );
        assert_eq!(s.active(t0 + SIDELINE_FOR), 0);
    }

    #[test]
    fn naming_an_account_puts_it_back_at_once() {
        let mut s = Sidelined::default();
        let t0 = Instant::now();
        s.mark("rnd", t0);
        s.clear("rnd");
        assert!(!s.contains("rnd", t0));
    }
}

#[cfg(test)]
mod extra_usage_tests {
    use super::*;

    /// Stepping off a full window is right only when the wall is real. With
    /// extra usage available Anthropic keeps serving past the cap and bills
    /// credits, so the account is not out - and rotating away from it moves a
    /// conversation for no reason, onto an account the user did not choose.
    /// Credits are a last resort, not a reason to stay. A capped account with
    /// credits still steps aside for one with free room - otherwise swapdex
    /// spends money while another account sits idle with quota to spare. When
    /// there is nowhere better, the proxy stays put anyway, and the credits carry
    /// that turn.
    #[test]
    fn credits_do_not_keep_a_capped_account_in_front() {
        for credits in [false, true] {
            assert!(
                over_threshold_with(Some(100.0), Some(55.0), 0.98, credits),
                "capped is capped (credits: {credits})"
            );
        }
    }

    /// Credits do not make a fresh account any less fresh, and they are not a
    /// reason to reconsider an account that was never near its limit.
    #[test]
    fn credits_change_nothing_below_the_threshold() {
        for credits in [false, true] {
            assert!(!over_threshold_with(Some(10.0), Some(20.0), 0.98, credits));
        }
    }
}

/// Which account to reach for when the current one is full.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Strategy {
    /// Most room left first. Spreads the load and keeps the largest buffer for a
    /// burst - the rule swapdex has always used.
    #[default]
    Roomiest,
    /// Soonest-resetting window first. Quota that is about to reset costs
    /// nothing to spend, and spending it leaves the long windows untouched;
    /// picking the roomiest account instead lets the nearly-reset one lapse
    /// unused. teamclaude prefers this, and claude-swap calls it `consume-first`.
    ConsumeFirst,
}

impl Strategy {
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "roomiest" | "best" => Some(Self::Roomiest),
            "consume-first" | "consume_first" | "soonest" => Some(Self::ConsumeFirst),
            _ => None,
        }
    }
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Roomiest => "roomiest",
            Self::ConsumeFirst => "consume-first",
        }
    }
}

/// Order candidates by the chosen strategy. An explicit rank still wins in both:
/// the user saying "prefer this one" outranks any measurement.
///
/// `resets_in` is seconds until the account's soonest window turns over. An
/// account with nothing measured sorts after measured ones under either
/// strategy - unmeasured is not "empty" and not "free".
pub fn order_by<T>(
    items: &mut [T],
    strategy: Strategy,
    rank: impl Fn(&T) -> usize,
    room: impl Fn(&T) -> Option<f64>,
    resets_in: impl Fn(&T) -> Option<i64>,
) {
    match strategy {
        Strategy::Roomiest => by_headroom(items, rank, room),
        Strategy::ConsumeFirst => items.sort_by(|a, b| {
            rank(a).cmp(&rank(b)).then_with(|| {
                // Anything with no room left is not a candidate at all, whatever
                // its reset says: spending "soonest" makes no sense when there is
                // nothing left to spend.
                let usable = |t: &T| room(t).is_none_or(|r| r > 0.0);
                match (usable(a), usable(b)) {
                    (true, false) => return std::cmp::Ordering::Less,
                    (false, true) => return std::cmp::Ordering::Greater,
                    _ => {}
                }
                match (resets_in(a), resets_in(b)) {
                    (Some(x), Some(y)) => x.cmp(&y), // soonest first
                    (Some(_), None) => std::cmp::Ordering::Less,
                    (None, Some(_)) => std::cmp::Ordering::Greater,
                    (None, None) => std::cmp::Ordering::Equal,
                }
            })
        }),
    }
}

/// How much MORE room a candidate needs before the session is moved onto it.
///
/// A cooldown alone stops a fast flip-flop but not a slow one: two accounts
/// either side of the line trade the session every time the timer lapses, and
/// each hop throws away a warm prompt cache. A margin says "only move for a
/// real improvement".
pub const HYSTERESIS_MARGIN: f64 = 10.0;

/// Is the candidate enough better than where we are to be worth the move?
///
/// Unmeasured on either side means the question cannot be answered, and the
/// answer is no - moving on a guess is how a session ends up somewhere nobody
/// chose.
pub fn worth_moving_to(
    current_room: Option<f64>,
    candidate_room: Option<f64>,
    margin: f64,
) -> bool {
    match (current_room, candidate_room) {
        (Some(now), Some(next)) => next - now >= margin,
        _ => false,
    }
}

#[cfg(test)]
mod strategy_tests {
    use super::*;

    /// (name, rank, room, seconds until its soonest window resets)
    type Acct = (&'static str, usize, Option<f64>, Option<i64>);

    fn order(strategy: Strategy, mut v: Vec<Acct>) -> Vec<&'static str> {
        order_by(&mut v, strategy, |a| a.1, |a| a.2, |a| a.3);
        v.into_iter().map(|a| a.0).collect()
    }

    #[test]
    fn roomiest_takes_the_account_with_the_most_left() {
        let v = vec![
            ("tight", 0, Some(5.0), Some(60)),
            ("roomy", 0, Some(80.0), Some(9_000)),
        ];
        assert_eq!(order(Strategy::Roomiest, v), ["roomy", "tight"]);
    }

    /// The point of consume-first: `soon` has less left, but its window turns
    /// over in a minute, so spending it costs nothing and leaves `roomy` intact.
    /// Roomiest would let that minute's worth lapse unused.
    #[test]
    fn consume_first_takes_the_window_about_to_reset() {
        let v = vec![
            ("roomy", 0, Some(80.0), Some(9_000)),
            ("soon", 0, Some(30.0), Some(60)),
        ];
        assert_eq!(order(Strategy::ConsumeFirst, v), ["soon", "roomy"]);
    }

    /// "Soonest" is meaningless for an account with nothing left to spend.
    #[test]
    fn consume_first_still_skips_an_account_with_nothing_in_it() {
        let v = vec![
            ("empty", 0, Some(0.0), Some(30)),
            ("has-some", 0, Some(20.0), Some(9_000)),
        ];
        assert_eq!(order(Strategy::ConsumeFirst, v), ["has-some", "empty"]);
    }

    #[test]
    fn an_explicit_rank_outranks_both_strategies() {
        for s in [Strategy::Roomiest, Strategy::ConsumeFirst] {
            let v = vec![
                ("second", 1, Some(99.0), Some(10)),
                ("preferred", 0, Some(1.0), Some(99_999)),
            ];
            assert_eq!(order(s, v)[0], "preferred", "{s:?}");
        }
    }

    #[test]
    fn unmeasured_accounts_sort_after_measured_ones() {
        for s in [Strategy::Roomiest, Strategy::ConsumeFirst] {
            let v = vec![
                ("unknown", 0, None, None),
                ("known", 0, Some(50.0), Some(500)),
            ];
            assert_eq!(order(s, v)[0], "known", "{s:?}");
        }
    }

    #[test]
    fn the_names_round_trip() {
        for s in [Strategy::Roomiest, Strategy::ConsumeFirst] {
            assert_eq!(Strategy::parse(s.as_str()), Some(s));
        }
        assert_eq!(Strategy::parse("best"), Some(Strategy::Roomiest));
        assert_eq!(Strategy::parse("nonsense"), None);
    }
}

#[cfg(test)]
mod hysteresis_tests {
    use super::*;

    /// Two accounts either side of the line traded the session every time the
    /// cooldown lapsed, and every hop cost a warm prompt cache. A move now has
    /// to buy something.
    #[test]
    fn a_marginal_improvement_is_not_worth_the_move() {
        assert!(!worth_moving_to(Some(8.0), Some(12.0), HYSTERESIS_MARGIN));
        assert!(worth_moving_to(Some(8.0), Some(40.0), HYSTERESIS_MARGIN));
    }

    #[test]
    fn moving_to_something_worse_is_never_worth_it() {
        assert!(!worth_moving_to(Some(50.0), Some(20.0), HYSTERESIS_MARGIN));
    }

    /// Unmeasured cannot be compared, and moving on a guess is how a session
    /// lands somewhere nobody chose.
    #[test]
    fn an_unmeasured_side_is_not_a_reason_to_move() {
        assert!(!worth_moving_to(None, Some(90.0), HYSTERESIS_MARGIN));
        assert!(!worth_moving_to(Some(1.0), None, HYSTERESIS_MARGIN));
    }
}

/// How long to wait before measuring an account's usage again.
///
/// A fixed interval reads every account at the same rate, which is wrong at both
/// ends: an account near its limit is the one whose number matters and it goes
/// stale between reads, while an account at 3% is asked over and over for an
/// answer that will not change. Reading them all on one clock is also what got
/// the usage endpoint to rate-limit us during a survey.
///
/// So the interval follows how close the account is to mattering.
pub fn measure_after(headroom: Option<f64>) -> std::time::Duration {
    let secs = match headroom {
        // Never measured: find out soon, because a threshold cannot apply to an
        // account nobody has read.
        None => 60,
        // Nearly out - this is the number a rotation will turn on.
        Some(h) if h <= 10.0 => 60,
        Some(h) if h <= 25.0 => 120,
        Some(h) if h <= 50.0 => 300,
        // Plenty left. Asking again in two minutes buys nothing.
        _ => 900,
    };
    std::time::Duration::from_secs(secs)
}

#[cfg(test)]
mod pacing_tests {
    use super::*;

    #[test]
    fn a_nearly_spent_account_is_watched_closely() {
        assert!(measure_after(Some(5.0)) <= std::time::Duration::from_secs(60));
    }

    #[test]
    fn a_fresh_one_is_left_alone_far_longer() {
        assert!(measure_after(Some(95.0)) >= std::time::Duration::from_secs(600));
    }

    /// Monotone: more room can never mean a shorter wait. Without this the
    /// pacing could invert at a boundary and hammer exactly the account that
    /// needs it least.
    #[test]
    fn more_room_never_means_a_shorter_wait() {
        let mut last = std::time::Duration::ZERO;
        for h in [0.0, 10.0, 25.0, 50.0, 75.0, 100.0] {
            let d = measure_after(Some(h));
            assert!(d >= last, "wait shrank at {h}%: {last:?} -> {d:?}");
            last = d;
        }
    }

    /// An unmeasured account is not "plenty left" - the threshold cannot apply
    /// to a number nobody has, so it is read soon rather than last.
    #[test]
    fn an_unmeasured_account_is_read_soon() {
        assert_eq!(measure_after(None), measure_after(Some(0.0)));
    }
}
