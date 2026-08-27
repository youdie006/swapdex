//! Proxy mode: a loopback HTTP server that forwards Claude Code's API traffic
//! upstream, choosing the account per request so a running conversation can
//! change accounts. Credentials are read from slots and never copied, and
//! neither prompt content nor any token value is ever logged.

pub mod codex;
pub mod creds;
pub mod identity;
pub mod pick;
pub mod ratelimit;
pub mod upstream;

use crate::paths::Paths;
use anyhow::{anyhow, Result};
use std::collections::HashMap;
use std::io::Write;
use std::sync::{Arc, Mutex};

/// Take a lock, recovering the value if a previous holder panicked.
///
/// Every Mutex in this proxy guards a cache, a counter, or a note - never an
/// invariant that a half-finished update could leave unsafe. A thread that
/// panics while holding one poisons it forever, and `.unwrap()` on the other
/// side would turn a single dead background thread into a proxy that listens
/// happily and fails every request. Stale numbers are the better failure.
trait Held<T> {
    fn held(&self) -> std::sync::MutexGuard<'_, T>;
}

impl<T> Held<T> for std::sync::Mutex<T> {
    fn held(&self) -> std::sync::MutexGuard<'_, T> {
        self.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

/// Everything the request threads share: the upstream client, where upstream is,
/// the per-account quota state, and the account-choice state. One struct so a
/// request handler keeps a small signature as the proxy grows.
/// One account's measured state. A struct rather than a tuple because it now
/// carries four unrelated facts, and `(a, b, c, d)` at a call site says none of
/// them.
#[derive(Clone, Copy, Debug, Default)]
struct Measurement {
    /// Percentages USED, either of which may be unmeasured.
    five_h: Option<f64>,
    seven_d: Option<f64>,
    /// Can it keep serving once those are full? True when extra usage is enabled
    /// and its spend cap is not reached. Without this a full window read as a
    /// wall, and the proxy stepped off an account answering turns perfectly well.
    credits: bool,
    /// When each window turns over, epoch seconds. Kept SEPARATELY: they answer
    /// different questions - the 5h says when this afternoon frees up, the 7d
    /// says which day the account is out until - and collapsing them to the
    /// soonest one threw the second away.
    five_h_reset: Option<i64>,
    seven_d_reset: Option<i64>,
    /// When this reading was taken, so the NEXT one can be paced per account:
    /// one near its limit is worth watching, one at 3% is not.
    taken: Option<std::time::Instant>,
}

impl Measurement {
    /// Seconds until the SOONEST window resets, relative to `now`. What
    /// `consume-first` sorts on: quota about to reset costs nothing to spend.
    fn resets_in(&self, now: i64) -> Option<i64> {
        [self.five_h_reset, self.seven_d_reset]
            .into_iter()
            .flatten()
            .min()
            .map(|r| r - now)
    }
}

type Utilization = Measurement;

/// Measured utilization per account, with when the reading was taken.
type Measured = (Option<std::time::Instant>, HashMap<String, Utilization>);

struct Shared {
    agent: ureq::Agent,
    base: String,
    /// Last seen quota state per account name.
    /// What each account's last response said about its windows, and when that
    /// was recorded. The timestamp is what lets a refusal that named no reset
    /// lapse: without it, `rejected` meant "for the life of this proxy".
    quota: Mutex<HashMap<String, (ratelimit::Quota, i64)>>,
    chooser: Mutex<pick::Chooser>,
    /// The proxy's own current account after a rotation, if any.
    rotated: Mutex<Option<String>>,
    /// Accounts the upstream refused outright (401): not a quota problem, so kept
    /// apart from quota state, but equally out of rotation for this run.
    /// Accounts a refusal put out of the rotation, and when. NOT permanent: the
    /// remedy the proxy prints is "sign it in again", and a set with no removal
    /// went on skipping the account after the user had done exactly that.
    unusable: Mutex<pick::Sidelined>,
    /// Measured utilization per account (5h, 7d percentages) from the zero-spend
    /// usage endpoint, with when it was read. Only used when a threshold is set.
    measured: Mutex<Measured>,
    /// Every account is past the threshold and there is nowhere to move: the one
    /// state in which asking for a cheaper model beats failing the turn.
    /// Why there is nowhere to send a turn, when there is nowhere - the two
    /// ways into that corner are different news and must not be reported alike.
    cornered: Mutex<Option<pick::Corner>>,
    /// The corner already announced, so a fallback that lasts a while does not
    /// repeat one sentence on every turn.
    corner_note: Mutex<Option<pick::Corner>>,
    /// When each account last REFUSED a turn, and when one last succeeded,
    /// unix seconds. The refusal record in `quota` lapses on purpose - a rate
    /// limit is a window, not a verdict - but the "(on credits)" label must not
    /// come back with it before anything has actually gone through.
    refused_at: Mutex<HashMap<String, i64>>,
    ok_at: Mutex<HashMap<String, i64>>,
    /// One refresh per slot per burst. Refresh tokens rotate, so N concurrent
    /// 401s each renewing the same slot spend the same token N times and all
    /// but one result is stale on arrival - the account logs itself out.
    /// When each account's credential was last replaced. A refusal belongs to
    /// the credential that earned it, so a token refresh retires it - without
    /// this, a re-authorized account stayed sidelined for a dead reason.
    replaced_at: Mutex<HashMap<String, i64>>,
    /// Said once when a Codex response turns out to state its own windows.
    /// Whether Codex sends those headers is undocumented and was never checked
    /// here, so the first arrival is worth seeing - and its absence stays
    /// visible as the line never appearing.
    codex_headers_seen: std::sync::atomic::AtomicBool,
    /// The last redirection announced, so a `serve` pointer stuck on a benched
    /// account does not print the same sentence on every turn.
    benched_note: Mutex<Option<(String, String)>>,
    /// When the last pre-emptive move happened. Threshold switching without a
    /// cooldown flip-flops: two accounts hovering either side of the line hand the
    /// session back and forth, and every hop costs the prompt cache.
    last_preempt: Mutex<Option<std::time::Instant>>,
}

pub struct Opts {
    pub port: u16,
    pub account: Option<String>,
    /// Which tool's traffic this proxy carries. The two differ in where upstream
    /// is and in how a turn's account is expressed - Claude puts it in the body,
    /// Codex in a header pair - but not in anything else the proxy does.
    pub tool: String,
    /// Continue on another account when the current one is spent, when the
    /// command line SAID so. `None` means "whatever the setting says right now",
    /// re-read per request - so `swapdex auto on` reaches a proxy that is already
    /// running, instead of waiting for a restart nobody performs.
    pub auto: Option<bool>,
    /// Step off an account once a window reaches this fraction (0.98 = 98%),
    /// BEFORE it refuses a turn. `None` waits for the refusal instead, which costs
    /// one failed turn per wall. Needs one usage read per account, so it is opt-in.
    pub threshold: Option<f64>,
    /// Was the threshold given on the command line? If not, the setting is read
    /// per request, for the same reason.
    pub threshold_pinned: bool,
}

/// Auto-continue for THIS request: the flag when one was given, else the setting
/// as it stands now.
fn auto_now(flag: Option<bool>, setting: bool) -> bool {
    flag.unwrap_or(setting)
}

/// The two settings a request needs, read at the moment it is served rather than
/// once at startup: `swapdex auto on` and `swapdex threshold 0.8` must reach the
/// proxy already running, the way the pointers deciding who serves already do.
fn live(paths: &Paths, opts: &Opts) -> (bool, Option<f64>) {
    let cfg = crate::settings::load(paths);
    let auto = auto_now(opts.auto, cfg.auto());
    let threshold = if opts.threshold_pinned {
        opts.threshold
    } else {
        cfg.threshold()
    };
    (auto, threshold.filter(|_| auto))
}

/// Is this an authentication exchange rather than a turn?
///
/// An OAuth flow is between the user and the vendor: the code, the token
/// exchange, the revoke. swapdex must not touch it. Rewriting the Authorization
/// header on one means a sign-in is answered as whichever account the proxy
/// already holds - so signing in reported success and produced the wrong account,
/// and that happens when `/login` is typed INSIDE a running session, where the
/// proxy address is already in the environment and no launcher guard can see it.
fn is_auth_exchange(path: &str) -> bool {
    // Compare on segments so a path merely CONTAINING the word cannot match: the
    // exemption skips token injection, so it must stay narrow.
    let p = path.split('?').next().unwrap_or(path).to_ascii_lowercase();
    p.split('/')
        .any(|seg| matches!(seg, "oauth" | "login" | "logout" | "authorize" | "auth"))
}

/// Headers that must not be forwarded: hop-by-hop, or ones the HTTP client sets
/// itself from the body and connection.
fn skip_header(name: &str) -> bool {
    matches!(
        name.to_ascii_lowercase().as_str(),
        "host" | "connection" | "content-length" | "transfer-encoding" | "accept-encoding"
    )
}

/// Should this turn re-read the accounts' quota windows?
///
/// Reading the quota and rotating on it are different jobs. The refresh used to
/// sit inside `if auto`, so with rotation off nothing ever re-read the windows:
/// the cache aged, its readings expired at their reset times, and the usage
/// vanished from every screen. Turning rotation off should cost the rotation,
/// not the numbers.
///
/// Codex is excluded whatever the setting says: it has no zero-spend usage
/// endpoint of this kind, and asking one API about another's account is not a
/// reading.
pub fn should_measure(_auto: bool, tool: &str) -> bool {
    tool != "codex"
}

/// How often a run of accept failures may speak.
pub const ACCEPT_REPORT_SECS: i64 = 30;

/// Counts connections that died before the proxy could read them.
///
/// `server.recv()` errors were swallowed with `continue`, so a client whose
/// connection dropped mid-request left no trace at all - the log showed only
/// successes while the user watched "API error" repeatedly, and nothing
/// anywhere told the two apart. A failure nobody can see is the hardest kind
/// to fix.
///
/// Rate-limited, because a broken client can fail hundreds of times a second
/// and a log that floods is as unreadable as one that says nothing.
#[derive(Default)]
pub struct AcceptFailures {
    last_reported: Option<i64>,
    suppressed: u64,
}

impl AcceptFailures {
    /// True when this failure should be printed rather than counted silently.
    pub fn should_report(&mut self, now_secs: i64) -> bool {
        match self.last_reported {
            Some(t) if now_secs - t < ACCEPT_REPORT_SECS => {
                self.suppressed += 1;
                false
            }
            _ => {
                self.last_reported = Some(now_secs);
                true
            }
        }
    }

    /// How many were swallowed since the last report, and reset.
    pub fn take_suppressed(&mut self) -> u64 {
        std::mem::take(&mut self.suppressed)
    }
}

/// Should a TRANSPORT failure be retried, and after how long?
///
/// A 529 was retried but a dropped connection was not: `forward` returned an
/// error and it went straight up, so an ECONNRESET mid-flight surfaced to the
/// user as "API error" even though the very next attempt usually succeeds.
/// These are the same event seen from different layers - the server shedding
/// load - and both deserve the same patience.
///
/// Shorter than the 529 budget: a reset is answered immediately by the network
/// rather than after a server-side wait, so the retries can come faster.
pub fn transport_retry(attempt: u32) -> Option<std::time::Duration> {
    const MAX: u32 = 4;
    if attempt >= MAX {
        return None;
    }
    Some(std::time::Duration::from_millis(250u64 << attempt))
}

/// How long to wait before retrying an overloaded server, if at all.
///
/// 529 is Anthropic being overloaded, not this account being spent. It was
/// passed straight through, so the turn died and the user saw a connection
/// drop, 54 of them in one day's log. Waiting briefly usually clears it, and
/// switching accounts cannot: every account talks to the same server, so
/// rotating would drop the prompt cache for nothing.
///
/// Bounded at three tries: a server that stays down has to surface as an error
/// rather than hold the turn open indefinitely.
pub fn overload_retry(status: u16, attempt: u32) -> Option<std::time::Duration> {
    // Sized against real logs rather than a textbook curve: a 529 spell on this
    // API runs for about a MINUTE - eighteen of them inside one minute - so the
    // old 1s+2s+4s budget gave up seven seconds in and handed the user an error
    // while the wave was still passing. Eight tries backing off to a 15s
    // ceiling covers roughly a minute, and still ends rather than looping.
    const MAX: u32 = 8;
    const CEILING_SECS: u64 = 15;
    if status != 529 || attempt >= MAX {
        return None;
    }
    Some(std::time::Duration::from_secs(
        (1u64 << attempt).min(CEILING_SECS),
    ))
}

/// Whether this response is the account REFUSING, or just a moment mid-round.
///
/// `note_outcome` runs inside the retry loop, so a momentary throttle used to
/// stamp the account as refusing before the retry had even run. A successful
/// retry overwrote the stamp, but a round that ended any other way left the
/// account sidelined for having been briefly slow. Only the verdict that ENDS
/// the round is about the account - "count refusal rounds, not responses".
pub fn records_refusal(status: u16, will_retry_same_account: bool) -> bool {
    if will_retry_same_account {
        return false;
    }
    ratelimit::account_cannot_serve(status)
}

/// Remember what this account's last answer actually was.
///
/// `quota` already records a refusal, but it lapses by design, and the
/// "(on credits)" label was coming back with it - promising a way through that
/// had been tried and refused minutes earlier. These two stamps outlive the
/// lapse, so the promise waits for a turn that actually goes through.
fn note_outcome(sh: &Shared, name: &str, status: u16) {
    let at = now_secs();
    if ratelimit::account_cannot_serve(status) {
        sh.refused_at.held().insert(name.to_string(), at);
    } else if (200..300).contains(&status) {
        sh.ok_at.held().insert(name.to_string(), at);
    }
}

/// Headers that must not be echoed back to the client.
///
/// `content-encoding` above all. ureq negotiates gzip and DECODES the body on
/// the way in, so what this proxy hands on is plain bytes - but the upstream's
/// `content-encoding: gzip` was being passed along with them. The client then
/// tried to gunzip text that was already text and the stream died partway
/// through, which Claude Code reports as "Connection lost mid-response" and the
/// proxy log recorded as "gzip decompression failed".
///
/// Kept separate from `skip_header`, which filters what goes UP: a client may
/// legitimately send an encoded request body, and dropping the label there
/// would corrupt it in the other direction.
fn skip_response_header(name: &str) -> bool {
    matches!(
        name.to_ascii_lowercase().as_str(),
        "host"
            | "connection"
            | "content-length"
            | "transfer-encoding"
            // The length and the encoding both describe bytes that no longer
            // exist by the time they reach the client.
            | "content-encoding"
    )
}

/// The slot the next request should use. `--account` pins one absolutely;
/// otherwise the registry and the `active-claude` pointer are re-read PER
/// REQUEST, which is what lets `swapdex use <name>` (or Enter in the TUI) move a
/// conversation that is already running.
fn pick_slot(paths: &Paths, opts: &Opts, sh: &Arc<Shared>) -> Result<crate::slots::SlotRecord> {
    let slots = crate::slots::Slots::open_for(paths, &opts.tool)?;
    if let Some(name) = &opts.account {
        return slots
            .get(name)
            .ok_or_else(|| anyhow!("no account slot named '{name}' - `swapdex slots` lists them"));
    }
    let list = slots.list();
    // Who serves is its own answer when one was given: `swapdex serve <name>`
    // hands turns to an account without moving where sessions start, so a
    // conversation keeps living where it began while another account pays.
    let pointer = slots.serving_dir().or_else(|| slots.default_dir());
    let rotated = sh.rotated.held().clone();
    let chosen = sh
        .chooser
        .held()
        .choose(pointer.as_deref(), rotated.as_deref(), &list)
        .ok_or_else(|| anyhow!("no account slots yet - `swapdex run <name>` creates one"))?;
    // With --auto, an account already known to be out of quota should not serve
    // the next turn: the previous response said a window was spent, so start
    // elsewhere instead of walking into the wall. The turn that OBSERVED this was
    // still served by that account (rotating mid-turn would drop the prompt cache
    // for nothing), which is why the check belongs here and not there.
    let (auto, live_threshold) = live(paths, opts);
    // Read the windows whether or not rotation is on. This used to live inside
    // `if auto`, so with rotation off nothing re-read them: the cache aged, its
    // readings expired at their reset times, and the usage vanished from every
    // screen. Turning rotation off should cost the rotation, not the numbers.
    if should_measure(auto, &opts.tool) {
        refresh_measured(paths, &list, sh);
    }
    if auto {
        // Stepping off BEFORE the wall needs a reading, and the only zero-spend
        // reading that exists is Anthropic's usage endpoint. Codex has none, so
        // its accounts are moved when one actually refuses a turn - never by
        // asking one API about another's account.
        if let Some(t) = live_threshold.filter(|_| opts.tool != "codex") {
            refresh_measured(paths, &list, sh);
            let full = sh
                .measured
                .held()
                .1
                .get(&chosen.name)
                .is_some_and(|m| pick::over_threshold_with(m.five_h, m.seven_d, t, m.credits));
            // A move made moments ago stands: without this, two accounts either
            // side of the line trade the session back and forth.
            let cooling = sh
                .last_preempt
                .held()
                .is_some_and(|t| t.elapsed() < PREEMPT_COOLDOWN);
            if full && !cooling {
                match usable_under_threshold(paths, sh, &chosen.name, t) {
                    Some(better) => {
                        println!(
                            "{} is near its limit - starting this turn on {}",
                            chosen.name, better.name
                        );
                        std::io::stdout().flush().ok();
                        *sh.cornered.held() = None;
                        *sh.rotated.held() = Some(better.name.clone());
                        *sh.last_preempt.held() = Some(std::time::Instant::now());
                        return Ok(better);
                    }
                    // Staying put on a full account is the right call when every
                    // other one is full too - but silence here is indistinguishable
                    // from the threshold not working at all.
                    None => {
                        // WHY nothing else could take it, not merely that
                        // nothing could. The filter rejects an account for six
                        // different reasons and only one of them is the
                        // threshold; an account at 97% left and refusing is not
                        // near its limit, and saying it is sends the reader to
                        // a quota page where nothing is wrong.
                        let over: Vec<bool> = {
                            let m = sh.measured.held();
                            list.iter()
                                .filter(|r| r.name != chosen.name)
                                .filter_map(|r| m.1.get(&r.name))
                                .map(|m| {
                                    pick::over_threshold_with(m.five_h, m.seven_d, t, m.credits)
                                })
                                .collect()
                        };
                        let corner = pick::why_no_move(&over);
                        println!("{} - staying on {}", corner.describe(), chosen.name);
                        std::io::stdout().flush().ok();
                        // Nowhere to rotate. If a fallback model is configured,
                        // the request path may ask for it rather than let the
                        // turn hit the wall - the LAST thing swapdex tries,
                        // never the first, because rotating gives the user what
                        // they asked for and this does not.
                        *sh.cornered.held() = Some(corner);
                    }
                }
            }
        }
        let known_spent = sh
            .quota
            .held()
            .get(&chosen.name)
            .is_some_and(|(q, at)| q.still_spent_since(*at, now_secs()))
            || sh
                .unusable
                .held()
                .contains(&chosen.name, std::time::Instant::now())
            // A lapsed token cannot serve and cannot be refreshed from here, so
            // treat it the same as spent when choosing where to start.
            || creds::slot_token_expired(&chosen.config_dir, now_ms());
        if !known_spent {
            // Served without redirection: the episode is over, so its return is
            // worth announcing again.
            pick::clear_bench_note(&mut sh.benched_note.held());
        }
        if known_spent {
            if let Some(better) = next_account(paths, sh, std::slice::from_ref(&chosen.name)) {
                // Say it - once. This used to be the quietest path in the proxy:
                // the account the rotation had settled on was benched, every turn
                // fell back here, and the log showed only the fallback serving
                // turn after turn with no reason given. Saying it on EVERY turn
                // was the opposite mistake: a `serve` pointer stuck on a benched
                // account repeated one sentence until nobody read any of them.
                if pick::announce_bench(&mut sh.benched_note.held(), &chosen.name, &better.name) {
                    println!(
                        "{} is benched - turns go to {} until it comes back",
                        chosen.name, better.name
                    );
                    std::io::stdout().flush().ok();
                }
                return Ok(better);
            }
        }
    }
    Ok(chosen)
}

/// How long a utilization reading is trusted before being taken again. Long
/// enough that the proxy is not a stream of requests, short enough that a fast
/// burn is noticed before it hits the wall.
/// How often the measurement PASS runs. Each account then decides for itself
/// whether it is due (`pick::measure_after`), so this is only the shortest wait
/// any account could want - it is not how often any single one is read.
const MEASURE_EVERY: std::time::Duration = std::time::Duration::from_secs(60);

/// How often idle accounts are renewed so their refresh tokens never go stale.
///
/// An OAuth refresh token rotates when used; leave an account alone long enough
/// and only a browser sign-in brings it back. Three accounts on this machine died
/// that way and stayed dead a week. Half an hour is far more often than needed to
/// stay ahead of a six-hour window, and cheap: most sweeps renew nothing.
const KEEP_ALIVE_EVERY: std::time::Duration = std::time::Duration::from_secs(30 * 60);

/// Renew idle accounts on a timer, for as long as the proxy runs.
///
/// Its own thread rather than the request path: keeping accounts alive must not
/// depend on somebody sending a turn, and that is exactly the account it needs to
/// reach - the one nobody is using.
fn spawn_keep_alive(paths: &Paths, tool: &str) {
    if tool == "codex" {
        // Codex renews inside its own home and exposes no expiry swapdex can read.
        return;
    }
    let paths = paths.clone();
    std::thread::spawn(move || loop {
        std::thread::sleep(KEEP_ALIVE_EVERY);
        let slots: Vec<(String, std::path::PathBuf)> =
            match crate::slots::Slots::open_for(&paths, "claude-code") {
                Ok(s) => s
                    .list()
                    .into_iter()
                    .map(|r| (r.name, r.config_dir))
                    .collect(),
                Err(_) => continue,
            };
        let (renewed, failed) = crate::refresh::keep_alive_sweep(&slots, now_ms());
        for name in &renewed {
            println!("keep-alive: renewed {name}");
        }
        for (name, why) in &failed {
            println!("keep-alive: {}", why.remedy(name));
        }
        if !renewed.is_empty() || !failed.is_empty() {
            std::io::stdout().flush().ok();
        }
    });
}

/// How long a pre-emptive move stands before another can happen. Long enough that
/// two accounts near the line cannot trade the session between them, short enough
/// that a genuine second wall is still stepped over.
const PREEMPT_COOLDOWN: std::time::Duration = std::time::Duration::from_secs(300);

/// Read each account's utilization from the zero-spend usage endpoint, at most
/// once per `MEASURE_EVERY`. This is the same read `swapdex quota` performs, with
/// each account's own token; it spends no message quota.
fn refresh_measured(paths: &Paths, slots: &[crate::slots::SlotRecord], sh: &Arc<Shared>) {
    let first = {
        let mut m = sh.measured.held();
        match m.0 {
            Some(t) if t.elapsed() < MEASURE_EVERY => return,
            // Claim the slot BEFORE the work starts, so concurrent turns do not
            // each begin their own read of the same accounts.
            Some(_) => {
                m.0 = Some(std::time::Instant::now());
                false
            }
            None => true,
        }
    };
    // The FIRST read is worth waiting for: with nothing measured there is nothing
    // to steer by, and the turn would start on whichever account the pointer
    // happens to name - which is exactly the near-limit one the threshold exists
    // to step off. Every LATER read happens off the request path: refreshing means
    // waiting out the usage endpoint's throttling, which is seconds, and by then
    // there is a previous reading good enough to choose with.
    if first {
        measure_now(paths, slots, sh);
        return;
    }
    let slots: Vec<crate::slots::SlotRecord> = slots.to_vec();
    let sh = Arc::clone(sh);
    let paths = paths.clone();
    std::thread::spawn(move || measure_now(&paths, &slots, &sh));
}

/// Carry the last known readings across a restart.
///
/// The proxy used to start with nothing and ask the usage endpoint about every
/// account at once, however recently each had been read - and several accounts
/// arriving together is precisely the burst that endpoint throttles. Three
/// service restarts in an afternoon put every account on a real machine into
/// "usage endpoint throttled" simultaneously.
///
/// The cache already records WHEN each reading was taken, so its age survives
/// the restart: an account read moments ago is not due, one read an hour ago is.
/// `quota_cache::load` has already dropped any window that has since turned
/// over, so nothing stale is carried forward as if it were current.
fn seed_from_cache(cache: &crate::quota_cache::Cache, now: i64) -> HashMap<String, Measurement> {
    cache
        .iter()
        .map(|(name, e)| {
            // A stamp from the future (a clock that jumped, a file copied from
            // another machine) must not become an Instant ahead of now - that
            // account would never come due again.
            let age = (now - e.at).max(0) as u64;
            let taken = std::time::Instant::now()
                .checked_sub(std::time::Duration::from_secs(age))
                .unwrap_or_else(std::time::Instant::now);
            (
                name.clone(),
                Measurement {
                    five_h: e.five_h,
                    seven_d: e.seven_d,
                    credits: e.on_credits,
                    five_h_reset: e.five_h_reset,
                    seven_d_reset: e.seven_d_reset,
                    taken: Some(taken),
                },
            )
        })
        .collect()
}

/// The read itself. Records when it finished, not when it began, so a slow read
/// is not immediately due again.
/// Did a reading actually land this round?
///
/// The write-back restamps an account's stored value with the current time, and
/// its own comment says only what was READ may be restamped. The account was
/// being marked as read BEFORE the fetch, so a refused or offline round wrote
/// the previous number back looking brand new. An old number shown as current
/// is worse than an old number shown as old.
fn reading_landed(fetched: &crate::quota::Fetch) -> bool {
    matches!(fetched, crate::quota::Fetch::Ok(_))
}

fn measure_now(paths: &Paths, slots: &[crate::slots::SlotRecord], sh: &Shared) {
    // Carry forward what is still fresh enough. Reading every account on one
    // clock asks the fresh ones for an answer that has not changed, and it is
    // what got the usage endpoint to rate-limit us.
    let mut out: HashMap<String, Measurement> = {
        let m = sh.measured.held();
        m.1.clone()
    };
    let now = std::time::Instant::now();
    let mut unread: Vec<(String, String)> = Vec::new();
    // Which accounts this round actually asked about, so the write-back does
    // not restamp a carried-over value as if it had just been read.
    let mut just_read: std::collections::HashSet<String> = std::collections::HashSet::new();
    // What the accounts' own answers said, which can contradict what their
    // windows say. An account measured at 0% whose overage is spent refuses
    // every turn, and printing only the percentage offers a reserve that is not
    // there - the threshold hands it the session and it comes straight back.
    // Carry WHICH window closed, not just that one did. The response names it
    // ("overage-status"), and an account refusing with 90% of its windows left
    // is a contradiction until the reader is told the block is not about quota.
    let refused: Vec<(String, String)> = {
        let q = sh.quota.held();
        let now_s = now_secs();
        let by_headers: Vec<(String, String)> = slots
            .iter()
            .filter_map(|r| {
                let (quota, at) = q.get(&r.name)?;
                quota.still_spent_since(*at, now_s).then(|| {
                    let windows = quota
                        .rejected_windows()
                        .iter()
                        .map(|w| w.trim_end_matches("-status"))
                        .collect::<Vec<_>>()
                        .join(", ");
                    (r.name.clone(), windows)
                })
            })
            .collect();
        // The headers are not the only witness, and on this API they are the
        // rarer one: a 429 arrives with no `anthropic-ratelimit-unified-*` at
        // all, so an account that had just refused three turns still printed
        // "(on credits)" - offering a way through that had already failed.
        let now_i = std::time::Instant::now();
        // Three witnesses, because each misses what the others catch. The
        // headers name the window but only arrive on some refusals; the
        // sideline set holds 401/403; and the stamps catch the 429, which is
        // the commonest of the three and the only one neither other sees.
        let sidelined: Vec<String> = {
            let held = sh.unusable.held();
            let bad = sh.refused_at.held();
            let good = sh.ok_at.held();
            let fresh = sh.replaced_at.held();
            slots
                .iter()
                .filter(|r| {
                    held.contains(&r.name, now_i)
                        // A refusal belongs to the credential that earned it: a
                        // token refreshed since then retires it, so a
                        // re-authorized account is not held out for a dead
                        // reason until the lapse timer happens to expire.
                        || (pick::refusal_survives(
                            bad.get(&r.name).copied(),
                            good.get(&r.name).copied(),
                            fresh.get(&r.name).copied(),
                        ) && pick::currently_refusing(
                            bad.get(&r.name).copied(),
                            good.get(&r.name).copied(),
                            now_s,
                            ratelimit::SPENT_FOR_SECS,
                        ))
                })
                .map(|r| r.name.clone())
                .collect()
        };
        let names: Vec<String> = slots.iter().map(|r| r.name.clone()).collect();
        pick::refusing(&names, &by_headers, &sidelined)
    };
    for r in slots {
        if let Some(prev) = out.get(&r.name) {
            // A reading whose window has turned over describes a window that no
            // longer exists: the cache drops it on load and the screen goes
            // blank. Waiting out the headroom interval - fifteen minutes for an
            // account with plenty left - is what makes the numbers disappear
            // for a stretch after every reset.
            let outlived = pick::reading_outlived_its_window(
                prev.five_h_reset,
                prev.seven_d_reset,
                now_secs(),
            );
            let due = outlived
                || prev.taken.is_none_or(|t| {
                    now.duration_since(t)
                        >= pick::measure_after(pick::headroom(prev.five_h, prev.seven_d))
                });
            if !due {
                continue;
            }
        }
        // Both of these used to `continue` without a word, which is where the
        // account that serves fine can still vanish from this line: serving
        // RENEWS a lapsed token on the way past, and measuring does not - so an
        // account answering 200s all day can be unmeasurable, and nothing said so.
        // Ask for the REASON, not just the token. `slot_token` is the lossy
        // wrapper, and "not readable" covers two different situations - a
        // keychain that will not release the secret to this process, and a slot
        // with nothing signed in - whose remedies are opposites.
        let tok = match creds::slot_token_detail(&r.config_dir) {
            Ok(t) => t,
            Err(why) => {
                unread.push((r.name.clone(), why.short().to_string()));
                continue;
            }
        };
        let token = String::from_utf8_lossy(tok.expose()).to_string();
        if !crate::quota::token_usable(&token) {
            unread.push((
                r.name.clone(),
                "token lapsed - serving renews it, measuring does not".to_string(),
            ));
            continue;
        }
        crate::quota::pace_between_accounts();
        let fetched = crate::quota::fetch_with_retry(&token);
        // Count the account as read only when a reading actually LANDED. This
        // was inserted before the fetch, so a failed round wrote the previous
        // value back stamped with the current time - the one thing the
        // write-back comment says must not happen. An old number shown as
        // current is worse than an old number shown as old.
        if reading_landed(&fetched) {
            just_read.insert(r.name.clone());
        }
        if let Some(why) = fetched.why_no_number().map(str::to_string) {
            // Say WHY. A dropped read used to make the account vanish from the
            // usage line, and an account with no measurement cannot be held to
            // the threshold - so the one that silently disappears is the one
            // that stops stepping off before it hits a wall.
            unread.push((r.name.clone(), why));
        }
        if let crate::quota::Fetch::Ok(q) = fetched {
            out.insert(
                r.name.clone(),
                Measurement {
                    five_h: q.five_hour.map(|w| w.used_pct),
                    seven_d: q.seven_day.map(|w| w.used_pct),
                    credits: q.can_serve_past_windows(),
                    // The soonest of the two windows: that is the one whose
                    // quota is about to become free.
                    five_h_reset: q.five_hour.and_then(|w| w.resets_at),
                    seven_d_reset: q.seven_day.and_then(|w| w.resets_at),
                    taken: Some(std::time::Instant::now()),
                },
            );
        }
    }
    // Say what came back. A threshold that never fires because nothing could be
    // measured is indistinguishable from one that is working, and this read is
    // the only thing standing between the two.
    if out.is_empty() {
        let why = if unread.is_empty() {
            String::new()
        } else {
            format!(
                ": {}",
                pick::usage_line(
                    &[],
                    &unread,
                    &refused.iter().map(|(n, _)| n.clone()).collect::<Vec<_>>()
                )
            )
        };
        println!("  (no account's usage could be read{why} - the threshold cannot apply)");
    } else {
        // Each window on its own, both of them. The 5h and the 7d answer
        // different questions - when this afternoon frees up, versus which day
        // the account is out until - and one number that was the larger of the
        // two answered neither.
        let now_s = now_secs();
        let tz = tz_offset();
        let win = |pct: Option<f64>, at: Option<i64>, label: &str| -> Option<String> {
            let used = pct?;
            let when = at.map(|t| pick::reset_clock(t, now_s, tz));
            Some(pick::window_left(label, used, when))
        };
        let measured: Vec<(String, String)> = out
            .iter()
            .map(|(n, m)| {
                let honest = pick::still_offering_credits(
                    m.credits,
                    m.five_h,
                    m.seven_d,
                    sh.refused_at.held().get(n).copied(),
                    sh.ok_at.held().get(n).copied(),
                );
                let via = pick::credits_note(honest, refused.iter().any(|(r, _)| r == n));
                let parts: Vec<String> = [
                    win(m.five_h, m.five_h_reset, "5h"),
                    win(m.seven_d, m.seven_d_reset, "7d"),
                ]
                .into_iter()
                .flatten()
                .collect();
                // Say how old the number is once it stops being current. An
                // account read before its window filled showed "5h 100% left"
                // while it was in fact spent, and the line looked exactly like
                // one measured a second ago.
                let age = m
                    .taken
                    .and_then(|t| pick::reading_age_note(t.elapsed().as_secs()))
                    .map(|n| format!(" ({n})"))
                    .unwrap_or_default();
                let value = if parts.is_empty() {
                    "?".to_string()
                } else {
                    format!("{}{via}{age}", parts.join(" · "))
                };
                (n.clone(), value)
            })
            .collect();
        // An account that answered and one that could not be READ must not look
        // the same. Naming the unread ones keeps a partial round from reading as
        // a complete one - which is how the account actually serving dropped out
        // of this line unnoticed.
        // An account that has a number keeps it: a failed re-read does not
        // erase what was already known, and printing both made one account
        // appear twice on a line that then contradicted itself.
        println!("  usage:");
        for line in pick::usage_block(&measured, &unread, &refused) {
            println!("    {line}");
        }
    }
    std::io::stdout().flush().ok();
    // Write the readings back where the dashboard and the next restart can find
    // them. Only what was actually READ this round is written; a carried-over
    // value keeps its original age rather than being restamped as fresh.
    let fresh: Vec<(String, crate::quota_cache::Entry)> = out
        .iter()
        .filter(|(n, _)| just_read.contains(*n))
        .map(|(n, m)| {
            (
                n.clone(),
                crate::quota_cache::Entry {
                    five_h: m.five_h,
                    five_h_reset: m.five_h_reset,
                    seven_d: m.seven_d,
                    seven_d_reset: m.seven_d_reset,
                    at: now_secs(),
                    on_credits: m.credits,
                    refused: None,
                },
            )
        })
        .collect();
    if !fresh.is_empty() {
        crate::quota_cache::update(paths, &fresh);
    }
    let mut m = sh.measured.held();
    *m = (Some(std::time::Instant::now()), out);
}

/// An account that is signed in, allowed, and measured BELOW the threshold.
fn usable_under_threshold(
    paths: &Paths,
    sh: &Shared,
    current: &str,
    threshold: f64,
) -> Option<crate::slots::SlotRecord> {
    let mut slots = crate::slots::Slots::open(paths).map(|s| s.list()).ok()?;
    let cfg = crate::settings::load(paths);
    let measured = sh.measured.held();
    let spent = sh.quota.held();
    let unusable = sh.unusable.held();
    let now = std::time::Instant::now();
    let now_s = now_secs();
    // Ordered by the chosen strategy, so the session lands somewhere it can stay -
    // picking the first eligible account tends to land on one nearly full too.
    let room = |r: &crate::slots::SlotRecord| {
        measured
            .1
            .get(&r.name)
            .and_then(|m| pick::headroom(m.five_h, m.seven_d))
    };
    pick::order_by(
        &mut slots,
        cfg.strategy(),
        |r| cfg.rank(&r.name),
        room,
        |r| measured.1.get(&r.name).and_then(|m| m.resets_in(now_s)),
    );
    // Where we are now, so a move has to buy something. A cooldown alone stops a
    // fast flip-flop but not a slow one: two accounts either side of the line
    // trade the session every time the timer lapses, and each hop throws away a
    // warm prompt cache.
    let here = measured
        .1
        .get(current)
        .and_then(|m| pick::headroom(m.five_h, m.seven_d));
    slots.into_iter().find(|r| {
        r.name != current
            && !cfg.is_disabled(&r.name)
            && !unusable.contains(&r.name, now)
            && !spent
                .get(&r.name)
                .is_some_and(|(q, at)| q.still_spent_since(*at, now_s))
            && !measured
                .1
                .get(&r.name)
                .is_some_and(|m| {
                    pick::over_threshold_with(m.five_h, m.seven_d, threshold, m.credits)
                })
            && creds::slot_token(&r.config_dir).is_some()
            // Under consume-first the point IS to move to a smaller window, so
            // the margin would forbid the very move the strategy asks for.
            && (cfg.strategy() == pick::Strategy::ConsumeFirst
                || pick::worth_moving_to(here, room(r), pick::HYSTERESIS_MARGIN))
    })
}

/// Run `f` on SIGINT/SIGTERM, then exit. Used to drop the proxy marker when the
/// user Ctrl-Cs a foreground proxy. Best-effort: a hard kill still leaves the
/// marker, which the shim's pid check then ignores.
fn ctrl_c_cleanup<F: Fn() + Send + Sync + 'static>(f: F) -> Result<()> {
    use std::sync::OnceLock;
    static HOOK: OnceLock<Box<dyn Fn() + Send + Sync>> = OnceLock::new();
    let boxed: Box<dyn Fn() + Send + Sync> = Box::new(f);
    if HOOK.set(boxed).is_err() {
        return Ok(());
    }
    extern "C" fn on_signal(sig: libc::c_int) {
        if let Some(f) = HOOK.get() {
            f();
        }
        // Re-raise with the default handler so the exit status is honest.
        unsafe {
            libc::signal(sig, libc::SIG_DFL);
            libc::raise(sig);
        }
    }
    let handler = on_signal as extern "C" fn(libc::c_int) as *const () as libc::sighandler_t;
    unsafe {
        libc::signal(libc::SIGINT, handler);
        libc::signal(libc::SIGTERM, handler);
    }
    Ok(())
}

pub fn serve(paths: &Paths, opts: &Opts) -> Result<()> {
    crate::atomic::ensure_not_root()?;
    // Refuse before binding anything if there is nothing to serve WITH. A proxy
    // that can read no credential still answers, forwarding the client's own
    // login on every turn, and says nothing - it looks like it works while doing
    // nothing it exists to do. That state, started from an ssh session with a
    // locked Keychain, served for a full day before anyone noticed. Failing here
    // means the shim gets no port and the tool runs with no proxy, which is the
    // login the user already has, and it works.
    if opts.tool != "codex" {
        let reads: Vec<_> = crate::slots::Slots::open_for(paths, &opts.tool)
            .map(|s| {
                s.list()
                    .into_iter()
                    .map(|r| creds::slot_token_detail(&r.config_dir).map(|_| ()))
                    .collect()
            })
            .unwrap_or_default();
        if let Some(why) = creds::startup_refusal(&reads) {
            return Err(anyhow!("{why}"));
        }
    }
    // Loopback only: this holds a live credential, so it must never be
    // reachable off the machine.
    let server = match tiny_http::Server::http(("127.0.0.1", opts.port)) {
        Ok(s) => s,
        Err(e) => take_the_port(paths, &opts.tool, opts.port)
            .ok_or_else(|| anyhow!("cannot bind 127.0.0.1:{}: {e}", opts.port))?,
    };
    let port = server
        .server_addr()
        .to_ip()
        .ok_or_else(|| anyhow!("proxy did not get a TCP port"))?
        .port();
    // Announce the proxy so the installed `claude` shim points at it by itself -
    // "<pid> <port>", pid so a stale marker (killed proxy) is detectable.
    let marker = crate::shim::proxy_marker_for(paths, &opts.tool);
    let _ = std::fs::create_dir_all(paths.store_dir());
    // The marker carries the pid, the port, AND which build is serving. Updating
    // swapdex leaves the running proxy on the old code - which is how a fix can be
    // installed, verified, and still not be what answers the next request.
    let announced = std::fs::write(
        &marker,
        format!("{} {port} {}\n", std::process::id(), build_id()),
    )
    .is_ok();
    if announced {
        // Ctrl-C is the normal way to stop a foreground proxy, so clean up there
        // too - otherwise the shim would keep pointing at a dead port until the
        // pid check catches it.
        let m = marker.clone();
        // This tool's marker only: removing the shared one on exit also erased
        // the other proxy's, so the surviving proxy stopped being reported as
        // serving anything.
        let serving = serving_file_for(paths, &opts.tool);
        let _ = ctrl_c_cleanup(move || {
            let _ = std::fs::remove_file(&m);
            let _ = std::fs::remove_file(&serving);
        });
    }
    let is_codex = opts.tool == "codex";
    let bin = if is_codex { "codex" } else { "claude" };
    // The port ends this line: it is how the tests read it back, and anything
    // appended after it turns the port into an unparseable token.
    println!("swapdex {bin} proxy listening on http://127.0.0.1:{port}");
    if announced && crate::shim::shim_path_for(paths, &opts.tool).exists() {
        println!("  a plain `{bin}` now goes through it (the shim picks it up)");
    } else if is_codex {
        // Codex reaches a proxy through a model provider, not an env var, and
        // the block must declare no api key or Codex sends one instead of the
        // ChatGPT login this proxy switches between.
        println!("  point Codex at it:  codex -c model_provider=swapdex \\");
        println!("    -c model_providers.swapdex.name=swapdex \\");
        println!("    -c model_providers.swapdex.base_url=http://127.0.0.1:{port}/v1 \\");
        println!("    -c model_providers.swapdex.wire_api=responses");
    } else {
        println!("  point Claude at it:  export ANTHROPIC_BASE_URL=http://127.0.0.1:{port}");
    }
    // Say what this proxy will actually do. It is usually started in the
    // background by the shim, so its settings are otherwise invisible - and a
    // threshold that silently failed to load looks exactly like one that is
    // working.
    // Read the same way a request reads them, and said to be what they are NOW -
    // they are consulted again on every turn, so this line is a snapshot, not a
    // promise for the life of the process.
    let (auto_now_, thr_now) = live(paths, opts);
    match (auto_now_, thr_now.filter(|_| !is_codex)) {
        (true, Some(t)) => println!(
            "  auto: hands the session on at {:.0}% used, or when an account refuses",
            (t * 100.0).round()
        ),
        (true, None) => println!("  auto: hands the session on when an account refuses"),
        (false, _) => println!("  auto is off - `swapdex auto on` lets it move by itself"),
    }
    std::io::stdout().flush().ok();

    spawn_keep_alive(paths, &opts.tool);

    let server = Arc::new(server);
    let sh = Arc::new(Shared {
        codex_headers_seen: std::sync::atomic::AtomicBool::new(false),
        refused_at: Mutex::new(HashMap::new()),
        ok_at: Mutex::new(HashMap::new()),
        replaced_at: Mutex::new(HashMap::new()),
        agent: upstream::agent(),
        base: if opts.tool == "codex" {
            codex::base_url()
        } else {
            upstream::base_url()
        },
        quota: Mutex::new(HashMap::new()),
        chooser: Mutex::new(pick::Chooser::default()),
        rotated: Mutex::new(None),
        unusable: Mutex::new(pick::Sidelined::default()),
        cornered: Mutex::new(None),
        corner_note: Mutex::new(None),
        benched_note: Mutex::new(None),
        // Start from what was last known rather than from nothing: see
        // `seed_from_cache`. Without this every restart re-asked the endpoint
        // about every account at once.
        measured: Mutex::new((
            None,
            seed_from_cache(&crate::quota_cache::load(paths), now_secs()),
        )),
        last_preempt: Mutex::new(None),
    });

    // Keep the readings current on a timer, not only when a request happens to
    // arrive. Measurement used to ride along with traffic, so a quiet stretch
    // froze every number on screen at whatever it was when the last turn ran -
    // the status bar showed a reading fifteen minutes old and looked broken.
    if should_measure(true, &opts.tool) {
        let paths_m = paths.clone();
        let sh_m = Arc::clone(&sh);
        let tool_m = opts.tool.clone();
        std::thread::spawn(move || loop {
            std::thread::sleep(MEASURE_EVERY);
            if let Ok(sl) = crate::slots::Slots::open_for(&paths_m, &tool_m) {
                refresh_measured(&paths_m, &sl.list(), &sh_m);
            }
        });
    }

    let mut accept_failures = AcceptFailures::default();
    loop {
        let rq = match server.recv() {
            Ok(r) => r,
            // A connection that died before it could be read used to vanish
            // here. The log then showed only successes while the user watched
            // "API error" repeatedly, with nothing anywhere to tell the two
            // apart. Rate-limited so a broken client cannot flood it.
            Err(e) => {
                if accept_failures.should_report(now_secs()) {
                    let also = accept_failures.take_suppressed();
                    if also > 0 {
                        println!("  a client connection dropped before it could be read ({e}) - and {also} more since the last note");
                    } else {
                        println!("  a client connection dropped before it could be read ({e})");
                    }
                    std::io::stdout().flush().ok();
                }
                continue;
            }
        };
        let paths = paths.clone();
        let sh = sh.clone();
        let opts = Opts {
            port,
            account: opts.account.clone(),
            tool: opts.tool.clone(),
            auto: opts.auto,
            threshold: opts.threshold,
            threshold_pinned: opts.threshold_pinned,
        };
        std::thread::spawn(move || {
            // A failure here is the user's problem to see: answering with a bare
            // 500 tells them only that something broke, and the client shows
            // "500 (no body)" while the reason - a login that could not be read,
            // usually - sits in a log they cannot reach.
            if let Err(e) = handle(rq, &paths, &opts, &sh) {
                eprintln!("swapdex proxy: {e:#}");
            }
        });
    }
}

/// The next account that can serve this turn: signed in, not already tried on
/// this turn, and not known to be out of quota or refused. `None` when nothing is
/// left, which the caller reports rather than looping.
fn next_account_for(
    paths: &Paths,
    opts: &Opts,
    sh: &Shared,
    tried: &[String],
) -> Option<crate::slots::SlotRecord> {
    next_account_in(paths, &opts.tool, sh, tried)
}

fn next_account(paths: &Paths, sh: &Shared, tried: &[String]) -> Option<crate::slots::SlotRecord> {
    next_account_in(paths, "claude-code", sh, tried)
}

fn next_account_in(
    paths: &Paths,
    tool: &str,
    sh: &Shared,
    tried: &[String],
) -> Option<crate::slots::SlotRecord> {
    let mut slots = crate::slots::Slots::open_for(paths, tool)
        .map(|s| s.list())
        .ok()?;
    let cfg = crate::settings::load(paths);
    // Explicit order first (a ranked account is one the user said to prefer),
    // then most room left - handing the session to whichever account happened to
    // be listed next tends to land on one that is nearly spent too.
    {
        let measured = sh.measured.held();
        pick::by_headroom(
            &mut slots,
            |r| cfg.rank(&r.name),
            |r| {
                measured
                    .1
                    .get(&r.name)
                    .and_then(|m| pick::headroom(m.five_h, m.seven_d))
            },
        );
    }
    let spent = sh.quota.held();
    let unusable = sh.unusable.held();
    let now = std::time::Instant::now();
    let now_s = now_secs();
    // Reduce each slot to the facts the choice turns on, then let pick decide.
    // The rule that one ACCOUNT can sit in two slots lives there, where it is
    // tested: a rate limit belongs to the account, so handing the next turn to a
    // twin directory is a rotation that looks like one and buys nothing.
    let candidates: Vec<pick::Candidate> = slots
        .iter()
        .map(|r| pick::Candidate {
            name: r.name.clone(),
            uuid: creds::slot_account_uuid(&r.config_dir),
            ruled_out: tried.contains(&r.name)
                || unusable.contains(&r.name, now)
                || spent
                    .get(&r.name)
                    .is_some_and(|(q, at)| q.still_spent_since(*at, now_s))
                // "Disabled" means do not pick this one FOR me; switching to it
                // by hand still works, which is why the check lives here and not
                // in pick_slot.
                || cfg.is_disabled(&r.name),
            // Never offer a slot that was never signed into, or whose token has
            // already lapsed: either one just earns a 401, and nothing here can
            // refresh it. What a login LOOKS like differs per tool, and asking
            // the Claude question about a Codex slot answered "no login" for
            // every one of them - so a refused Codex turn had nowhere to go.
            usable: has_usable_login(tool, &r.config_dir),
        })
        .collect();
    let chosen = pick::next_usable(&candidates)?.name.clone();
    slots.into_iter().find(|r| r.name == chosen)
}

/// Is there a login in this slot at all - asked WITHOUT touching it?
///
/// Separate from `has_usable_login` on purpose: that one renews a lapsed Claude
/// token as a side effect, which is right when a turn is about to be served and
/// wrong for anything merely asking a question. A label or a guard that renewed
/// tokens would rotate a refresh token behind the user's back, which is the
/// logout this project exists to prevent. A lapsed token still counts as a
/// login here, because it is renewable.
pub fn has_login(tool: &str, dir: &std::path::Path) -> bool {
    match tool {
        "codex" => codex::slot_auth(dir).is_some(),
        _ => login_present(creds::slot_token_detail(dir)),
    }
}

/// Does this reading mean the account is signed in?
///
/// A locked Keychain is not a missing login. On macOS the Claude credential
/// lives in the Keychain, and a shell that cannot open it - a remote one, a
/// non-interactive one - fails to read a token that is perfectly well there.
/// Reading that as "never signed in" refuses a working account and sends the
/// user to fix something that is not broken.
pub fn login_present(read: Result<crate::secret::Secret, creds::TokenUnavailable>) -> bool {
    !matches!(read, Err(creds::TokenUnavailable::NoLogin))
}

/// Can this slot serve a turn for `tool` right now?
fn has_usable_login(tool: &str, dir: &std::path::Path) -> bool {
    // A lapsed Claude token is renewable, so try before ruling the account out:
    // the accounts idle long enough to lapse are the ones with quota left.
    if tool != "codex" && creds::slot_token_expired(dir, now_ms()) {
        let _ = crate::refresh::refresh_slot(dir, now_ms());
    }
    match tool {
        // Codex refreshes its own token inside its home and records no expiry
        // swapdex can read, so the question is only whether a login is there.
        "codex" => codex::slot_auth(dir).is_some(),
        _ => creds::slot_token(dir).is_some() && !creds::slot_token_expired(dir, now_ms()),
    }
}

/// Unix seconds, for the rate-limit resets the API reports in that unit.
fn now_secs() -> i64 {
    now_ms() / 1000
}

/// Unix milliseconds, for expiry comparisons.
fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

fn handle(mut rq: tiny_http::Request, paths: &Paths, opts: &Opts, sh: &Arc<Shared>) -> Result<()> {
    // The request stays owned here so a failure can still be ANSWERED. Dropping it
    // gives the client a bare "500 (no body)" and the reason - a login that could
    // not be read, usually - stays in a log the user cannot reach.
    let up = match forward_turn(&mut rq, paths, opts, sh) {
        Ok(up) => up,
        Err(e) => {
            let msg = format!("{e:#}");
            let body = serde_json::json!({
                "type": "error",
                "error": { "type": "swapdex_proxy_error", "message": msg.clone() }
            })
            .to_string();
            let resp = tiny_http::Response::from_string(body)
                .with_status_code(tiny_http::StatusCode(502))
                .with_header(
                    tiny_http::Header::from_bytes(&b"content-type"[..], &b"application/json"[..])
                        .expect("static header"),
                );
            let _ = rq.respond(resp);
            return Err(anyhow!(msg));
        }
    };
    let out_headers: Vec<tiny_http::Header> = up
        .headers
        .iter()
        .filter(|(n, _)| !skip_response_header(n))
        .filter_map(|(n, v)| tiny_http::Header::from_bytes(n.as_bytes(), v.as_bytes()).ok())
        .collect();
    // The length is unknown (responses stream, and SSE has no length at all), so
    // answer chunked and let the reader drive.
    let resp = tiny_http::Response::new(
        tiny_http::StatusCode(up.status),
        out_headers,
        up.reader,
        None,
        None,
    );
    rq.respond(resp)?;
    Ok(())
}

/// Choose the account, serve the turn (retrying and rotating as needed), and hand
/// back the upstream response for the caller to relay.
fn forward_turn(
    rq: &mut tiny_http::Request,
    paths: &Paths,
    opts: &Opts,
    sh: &Arc<Shared>,
) -> Result<upstream::Upstream> {
    // The client's request, read once and reusable: serving the same turn on
    // another account means sending these bytes again with a different token.
    // Keep the client's own Authorization: if swapdex cannot supply a login, the
    // honest fallback is to send what Claude itself would have sent.
    let client_auth = rq
        .headers()
        .iter()
        .find(|h| h.field.equiv("authorization"))
        .map(|h| h.value.as_str().to_string());
    let client_headers: Vec<(String, String)> = rq
        .headers()
        .iter()
        .filter(|h| !skip_header(h.field.as_str().as_str()))
        .filter(|h| !h.field.equiv("authorization"))
        .map(|h| {
            (
                h.field.as_str().as_str().to_string(),
                h.value.as_str().to_string(),
            )
        })
        .collect();
    let is_codex = opts.tool == "codex";
    let url = if is_codex {
        codex::upstream_url(&sh.base, rq.url())
    } else {
        format!("{}{}", sh.base, rq.url())
    };
    let method = rq.method().as_str().to_string();
    let path = rq.url().to_string();
    let mut client_body = Vec::new();
    rq.as_reader().read_to_end(&mut client_body)?;

    let known_uuids: Vec<String> = crate::slots::Slots::open(paths)
        .map(|s| {
            s.list()
                .iter()
                .filter_map(|r| creds::slot_account_uuid(&r.config_dir))
                .collect()
        })
        .unwrap_or_default();

    // Authentication is the user's own business with the vendor. Pass it straight
    // through with the credential the client sent - no account chosen, no token
    // injected, no identity rewritten - and say so, since a silent exemption in
    // the log would be indistinguishable from a turn nobody served.
    if is_auth_exchange(&path) {
        println!("  {method} {path} -> signing in, passed through untouched");
        std::io::stdout().flush().ok();
        let mut headers = client_headers.clone();
        if let Some(auth) = client_auth.clone() {
            headers.push(("authorization".into(), auth));
        }
        return upstream::forward(&sh.agent, &method, &url, &headers, &client_body);
    }

    let mut slot = pick_slot(paths, opts, sh)?;
    // The mark is NOT written here. Choosing a slot is not the same as paying
    // with it: when the chosen one has no usable login the proxy forwards the
    // CLIENT's own credential, and a mark written at the moment of choosing then
    // named an account that paid for nothing - for as long as it kept being
    // chosen. It is written where a credential is actually committed to.
    let mut tried: Vec<String> = Vec::new();
    // Retries of the CURRENT account after a throttle, counted so a wall is never
    // mistaken for a pause and retried forever.
    let mut attempt = 0u32;
    // Remembered across the loop so the exit can shape a refusal it could not
    // rotate around: the last account tried, and whether anything else could have
    // taken the turn.
    let mut refused_by: Option<String> = None;
    let up = loop {
        // An already-lapsed token earns a 401 and cannot be refreshed from here,
        // so treat it exactly like having no login: step aside rather than spend
        // the turn proving it.
        // An expired access token is renewable, and until now it was simply
        // stepped over - which retired the accounts with the most quota left.
        // Renewing is skipped when the tool is running in that slot: see
        // refresh's module note.
        // One renewal per slot per burst. Refresh tokens rotate, so N concurrent
        // turns each renewing this slot spend the same token N times and all but
        // one result is stale on arrival - the account logs itself out by its own
        // renewal. A caller that stands down here simply uses the credential the
        // winner is about to write.
        // The claim used to live in this condition, so the two other paths that
        // reach `refresh_slot` - the keep-alive sweep and `has_usable_login` -
        // spent the token unguarded. It now lives inside `refresh_slot` itself,
        // which is why there is no claim here: claiming twice would make this
        // caller stand down against itself.
        if creds::slot_token_expired(&slot.config_dir, now_ms()) {
            match crate::refresh::refresh_slot(&slot.config_dir, now_ms()) {
                Ok(()) => {
                    // A new credential: any refusal the OLD one earned is not
                    // about this one, so record when it was replaced.
                    sh.replaced_at.held().insert(slot.name.clone(), now_secs());
                    println!("  {}: renewed its login", slot.name)
                }
                // Another turn is renewing it; the credential it writes is the
                // one this turn will use. Not worth a line on the request path.
                Err(crate::refresh::RefreshError::AlreadyRefreshing) => {}
                Err(why) => println!("  {}", why.remedy(&slot.name)),
            }
            std::io::stdout().flush().ok();
        }
        if creds::slot_token_expired(&slot.config_dir, now_ms()) {
            println!(
                "{}: its login has expired - passing your own login through \
                 (`swapdex run {}` once refreshes it)",
                slot.name, slot.name
            );
            std::io::stdout().flush().ok();
            let mut headers = client_headers.clone();
            if let Some(auth) = client_auth.clone() {
                headers.push(("authorization".into(), auth));
            }
            return upstream::forward(&sh.agent, &method, &url, &headers, &client_body);
        }
        // Codex expresses the serving account as a header PAIR - the OAuth
        // bearer and the account id it belongs to - and sends no account
        // identity in the body at all, so there is nothing to align there.
        if is_codex {
            let Some(auth) = codex::slot_auth(&slot.config_dir) else {
                println!(
                    "account '{}' has no usable Codex login - passing your own through \
                     (`swapdex run {} --tool codex` once signs it in)",
                    slot.name, slot.name
                );
                std::io::stdout().flush().ok();
                note_client_serving(paths, &opts.tool);
                let mut headers = client_headers.clone();
                if let Some(a) = client_auth.clone() {
                    headers.push(("authorization".into(), a));
                }
                return upstream::forward(&sh.agent, &method, &url, &headers, &client_body);
            };
            let mut headers = client_headers.clone();
            codex::apply_auth(&mut headers, &auth);
            note_serving_for(paths, &opts.tool, &slot.name);
            // Retries of THIS account for a throttle, counted so a wall is not
            // mistaken for a pause and retried forever.
            // A dropped connection is the same event as a 529 seen one layer
            // down - the server shedding load - and the next attempt usually
            // succeeds. Returning the error straight up is what reached the
            // user as "API error"/ECONNRESET.
            let mut up = {
                let mut t = 0u32;
                loop {
                    match upstream::forward(&sh.agent, &method, &url, &headers, &client_body) {
                        Ok(u) => break u,
                        Err(e) => match transport_retry(t) {
                            Some(wait) => {
                                println!(
                                    "{} {path} -> connection lost, retrying in {}ms",
                                    slot.name,
                                    wait.as_millis()
                                );
                                std::io::stdout().flush().ok();
                                t += 1;
                                std::thread::sleep(wait);
                            }
                            None => return Err(e),
                        },
                    }
                }
            };
            // Decide the retry BEFORE recording anything: a 429 that is about
            // to be retried on this same account is not the account refusing,
            // and stamping it as one sidelined accounts for being briefly slow.
            let will_retry = up.status == 429
                && matches!(
                    ratelimit::classify_429(&up.headers, attempt),
                    ratelimit::Throttle::RetryAfter(_)
                );
            if records_refusal(up.status, will_retry) || (200..300).contains(&up.status) {
                note_outcome(sh, &slot.name, up.status);
            }
            match upstream::explain_failure(&mut up) {
                // Same reasoning as the Claude path: the API says why, and
                // three digits alone leave the user with nothing to act on.
                Some(why) => println!(
                    "  {} {} -> {} [{}] - {why}",
                    method, path, up.status, slot.name
                ),
                None => println!("  {} {} -> {} [{}]", method, path, up.status, slot.name),
            }
            std::io::stdout().flush().ok();
            // Codex states its own windows on the response, when it states them
            // at all. The reading costs nothing - the response is already here -
            // and it belongs to the account that SERVED this turn, so unlike a
            // transcript there is nothing to attribute.
            //
            // Whether Codex sends these headers is undocumented and had never
            // been checked here, so the first arrival says so once. Their
            // absence stays visible as that line never appearing.
            if crate::codex_usage::remember(paths, &slot.name, &up.headers, now_secs())
                && !sh
                    .codex_headers_seen
                    .swap(true, std::sync::atomic::Ordering::Relaxed)
            {
                println!(
                    "  {}: this response stated its own quota windows",
                    slot.name
                );
                std::io::stdout().flush().ok();
            }
            // A 429 wears two meanings, exactly as it does on the Claude side. A
            // THROTTLE ("slow down", x-should-retry) is fixed by waiting and
            // retrying THIS account; treating it as exhaustion gave the account
            // away over a pause of one second.
            if up.status == 429 {
                if let ratelimit::Throttle::RetryAfter(wait) =
                    ratelimit::classify_429(&up.headers, attempt)
                {
                    attempt += 1;
                    println!("  {} throttled - retrying in {:?}", slot.name, wait);
                    std::io::stdout().flush().ok();
                    std::thread::sleep(wait);
                    continue;
                }
            }
            // A refusal means this account cannot serve the turn. With --auto,
            // hand the SAME turn to another account rather than returning the
            // failure - that is the whole point of continuing a session.
            //
            // `--account` pins the run to one account: every turn is that
            // account's and a refusal is its own answer to give. Claude's path
            // checked the pin before rotating and this one did not, so a pinned
            // run quietly billed a different account the moment it was refused.
            if ratelimit::account_cannot_serve(up.status)
                && live(paths, opts).0
                && opts.account.is_none()
            {
                tried.push(slot.name.clone());
                if up.status == 401 || up.status == 403 {
                    sh.unusable
                        .held()
                        .mark(&slot.name, std::time::Instant::now());
                } else if ratelimit::proven_spent(&up.headers, attempt) {
                    // Moving the turn along needs only a refusal; holding the
                    // account out of the rotation for a quarter of an hour needs
                    // the response to say WHY, or to keep saying no.
                    let mut spent = sh.quota.held();
                    let e = spent.entry(slot.name.clone()).or_default();
                    e.0.rejected = true;
                    e.1 = now_secs();
                }
                if let Some(next) = next_account_for(paths, opts, sh, &tried) {
                    println!("  {} is out - continuing on {}", slot.name, next.name);
                    std::io::stdout().flush().ok();
                    *sh.rotated.held() = Some(next.name.clone());
                    note_serving_for(paths, &opts.tool, &next.name);
                    slot = next;
                    continue;
                }
            }
            break up;
        }
        let token = match creds::slot_token_detail(&slot.config_dir) {
            Ok(t) => t,
            Err(why) => {
                // swapdex has no login to offer for this account. Rather than
                // failing the turn, get out of the way: forward what the CLIENT
                // sent, which is the login Claude would have used with no proxy at
                // all. Being unable to help is not a reason to break the tool.
                println!(
                    "{} - passing your own login through",
                    why.remedy(&slot.name)
                );
                std::io::stdout().flush().ok();
                note_client_serving(paths, &opts.tool);
                let mut headers = client_headers.clone();
                if let Some(auth) = client_auth.clone() {
                    headers.push(("authorization".into(), auth));
                }
                return upstream::forward(&sh.agent, &method, &url, &headers, &client_body);
            }
        };
        let mut headers = client_headers.clone();
        headers.push((
            "authorization".into(),
            format!("Bearer {}", String::from_utf8_lossy(token.expose())),
        ));
        // Keep the account identity in the body consistent with the token serving
        // this turn: the client names the account the conversation started with.
        let mut body = client_body.clone();
        // Cornered - every account past the threshold, nothing to rotate to. A
        // configured fallback model is the last thing to try before the turn
        // walks into the wall, and it is announced, because the user asked for a
        // different model and is getting this one.
        let corner = *sh.cornered.held();
        if let Some(corner) = corner {
            if let Some(m) = crate::settings::load(paths).fallback_model.as_deref() {
                if let Some(swapped) = identity::swap_model(&body, m) {
                    // Once per episode, and naming the corner it actually is.
                    // Repeating it every turn buried the lines above that said
                    // what the accounts had left.
                    let mut note = sh.corner_note.held();
                    if *note != Some(corner) {
                        *note = Some(corner);
                        println!("  {} - asking for {m} instead", corner.describe());
                        std::io::stdout().flush().ok();
                    }
                    body = swapped;
                }
            }
        } else {
            *sh.corner_note.held() = None;
        }
        if let Some(serving) = creds::slot_account_uuid(&slot.config_dir) {
            if let Some(aligned) = identity::align_account(&body, &known_uuids, &serving) {
                body = aligned;
            }
        }

        note_serving_for(paths, &opts.tool, &slot.name);

        // Whether anything above actually CHANGED the client's request. Both
        // rewrites are guesses about what the server will accept, so if it
        // then calls the request malformed, they are the first suspect.
        let rewritten = body != client_body;
        let mut unrewritten_tries = 0u32;

        // A 429 wears two meanings. A THROTTLE ("slow down", x-should-retry) is
        // fixed by waiting and retrying this same account.
        let mut up = loop {
            let up = upstream::forward(&sh.agent, &method, &url, &headers, &body)?;
            // The server says the REQUEST is wrong, and swapdex is the only
            // thing that touched it. What the client wrote is known-good by
            // construction - it is what would have been sent with no proxy at
            // all - so spend one try on it rather than hand back a failure the
            // user cannot act on.
            if ratelimit::retry_unrewritten(up.status, rewritten, unrewritten_tries) {
                unrewritten_tries += 1;
                println!(
                    "{} {path} -> {} on a request swapdex rewrote - retrying as you wrote it",
                    slot.name, up.status
                );
                std::io::stdout().flush().ok();
                drop(up);
                body = client_body.clone();
                continue;
            }
            // 529 is the SERVER overloaded, not this account. Passing it
            // through killed the turn and read as a connection drop; waiting
            // briefly usually clears it, and rotating cannot - every account
            // talks to the same server.
            if let Some(wait) = overload_retry(up.status, attempt) {
                println!(
                    "{} {path} -> 529 overloaded, retrying in {}s",
                    slot.name,
                    wait.as_secs()
                );
                std::io::stdout().flush().ok();
                drop(up);
                attempt += 1;
                std::thread::sleep(wait);
                continue;
            }
            if up.status != 429 {
                break up;
            }
            match ratelimit::classify_429(&up.headers, attempt) {
                ratelimit::Throttle::RetryAfter(wait) => {
                    println!(
                        "{} {path} -> 429 throttled, retrying in {}s",
                        slot.name,
                        wait.as_secs()
                    );
                    std::io::stdout().flush().ok();
                    drop(up); // release this response before sleeping
                    attempt += 1;
                    std::thread::sleep(wait);
                }
                ratelimit::Throttle::Exhausted => {
                    // Every account spent is a wall whose LENGTH is known: the
                    // windows state their own reset times. Failing here ended
                    // an unattended run at the first wall; holding until the
                    // earliest reset lets it finish on its own. Off unless
                    // `hold_seconds` is set - a caller that would rather see
                    // the error than wait must be able to.
                    let cap = crate::settings::load(paths).hold_seconds.unwrap_or(0);
                    // Reset times specifically, not readings: `load` drops an
                    // account pinned at 100%, which is exactly the account a
                    // hold is about, so the data vanished when it was needed.
                    let resets = crate::quota_cache::resets_for(paths, &opts.tool);
                    match pick::hold_for(&resets, now_secs(), cap) {
                        Some(wait) => {
                            println!(
                                "{} {path} -> every account is spent; holding {}s for the \
                                 earliest window to reset",
                                slot.name,
                                wait.as_secs()
                            );
                            std::io::stdout().flush().ok();
                            drop(up);
                            std::thread::sleep(wait);
                            attempt = 0;
                            continue;
                        }
                        None => break up,
                    }
                }
            }
        };

        // Record what this response says about the account, and log it. A
        // rejected window on a SUCCESSFUL response is noted but not acted on:
        // the account is still serving, and rotating away would drop the
        // prompt cache (which is organization-scoped) for nothing.
        note_outcome(sh, &slot.name, up.status);
        // A refusal reaches the user as an error with no explanation unless the
        // reason is read off it here - the API sends one, it was simply never
        // looked at. The body is handed on to the client untouched.
        if let Some(why) = upstream::explain_failure(&mut up) {
            println!("{} {path} -> {} - {why}", slot.name, up.status);
            std::io::stdout().flush().ok();
        }
        let quota = ratelimit::from_headers(&up.headers);
        // Codex states its own windows on the response, when it states them at
        // all. That reading costs nothing - the response is already here - and
        // it belongs to the account that SERVED this turn, so unlike a
        // transcript there is nothing to attribute. It only arrives while the
        // user is working, which is why the endpoint reading still exists for
        // accounts sitting idle.
        match &quota {
            Some(q) if q.rejected => println!(
                "{} {path} -> {} ({} spent)",
                slot.name,
                up.status,
                q.rejected_windows().join(", ")
            ),
            _ => println!("{} {path} -> {}", slot.name, up.status),
        }
        std::io::stdout().flush().ok();
        if let Some(q) = quota {
            sh.quota.held().insert(slot.name.clone(), (q, now_secs()));
        }
        if up.status == 403 {
            println!(
                "{}: not entitled to serve this - a lapsed subscription, most likely.                  Holding it out so it cannot answer for the whole fleet.",
                slot.name
            );
            sh.unusable
                .held()
                .mark(&slot.name, std::time::Instant::now());
        }
        if up.status == 401 {
            println!(
                "{}: login no longer accepted - run `swapdex run {}` once to sign it in again",
                slot.name, slot.name
            );
            sh.unusable
                .held()
                .mark(&slot.name, std::time::Instant::now());
        }
        if !ratelimit::account_cannot_serve(up.status) {
            break up;
        }

        // The wall (or a dead login). Serve THIS turn on another account rather
        // than handing the client a failure - that is what "continue the session
        // elsewhere" has to mean. Without --auto, or with nothing left to try,
        // the client gets the real response.
        if up.status == 429 && ratelimit::proven_spent(&up.headers, attempt) {
            let mut spent = sh.quota.held();
            let e = spent.entry(slot.name.clone()).or_default();
            e.0.rejected = true;
            e.1 = now_secs();
        }
        if !live(paths, opts).0 || opts.account.is_some() {
            refused_by = Some(slot.name.clone());
            break up;
        }
        tried.push(slot.name.clone());
        // Cornered by refusal rather than by measurement: every account has now
        // said no to THIS turn. Same corner, and it needs no usage reading.
        *sh.cornered.held() = next_account(paths, sh, &tried)
            .is_none()
            .then_some(pick::Corner::AllRefused);
        match next_account(paths, sh, &tried) {
            Some(next) => {
                println!(
                    "{} cannot serve this turn - retrying on {}",
                    slot.name, next.name
                );
                std::io::stdout().flush().ok();
                *sh.rotated.held() = Some(next.name.clone());
                drop(up); // discard the failed response; the retry replaces it
                slot = next;
            }
            None => {
                // Nothing swapdex offers can serve this turn. Before failing,
                // fall back to the login the CLIENT sent - that is what Claude
                // would have used with no proxy, and it is the difference between
                // "your accounts are all spent" and "you cannot work".
                if let Some(auth) = client_auth.clone() {
                    println!(
                        "{}: no account of mine can serve this - passing your own login through",
                        slot.name
                    );
                    std::io::stdout().flush().ok();
                    drop(up);
                    let mut headers = client_headers.clone();
                    headers.push(("authorization".into(), auth));
                    return upstream::forward(&sh.agent, &method, &url, &headers, &client_body);
                }
                // Nothing left to try. Say WHY in a way the client will render:
                // a bare 401 relayed from upstream reads as "log in to Claude",
                // when the fix is to re-run one account so its token refreshes.
                let names: Vec<String> = crate::slots::Slots::open(paths)
                    .map(|s| s.list().into_iter().map(|r| r.name).collect())
                    .unwrap_or_default();
                let held_out = sh.unusable.held().active(std::time::Instant::now());
                if held_out > 0 && held_out >= names.len().max(1) {
                    let first = names.first().cloned().unwrap_or_else(|| "<name>".into());
                    return Err(anyhow!(
                        "every account's login has expired. Run `swapdex run {first}` once \
                         (its own login refreshes there), then try again - swapdex does not \
                         mint tokens itself."
                    ));
                }
                println!("{}: no other account can serve this turn", slot.name);
                std::io::stdout().flush().ok();
                refused_by = Some(slot.name.clone());
                break up;
            }
        }
    };

    // A 429 we could not rotate around still goes back as a 429 - it is true. But
    // Claude Code reads a `Retry-After` over 20s as "cool down for thirty
    // minutes", and half an hour is absurd when the user can press Enter and be
    // on another account in seconds. Cap it only when there IS another account:
    // with nowhere to go, the real wait is the useful one.
    let mut up = up;
    if up.status == 429 {
        if let Some(name) = refused_by {
            let tried = [name];
            let somewhere = next_account(paths, sh, &tried).is_some();
            if somewhere {
                println!(
                    "  another account could take this - telling the client to retry in {}s                      rather than let it cool down for 30 minutes",
                    ratelimit::CLIENT_SLEEPS_UP_TO_SECS
                );
                std::io::stdout().flush().ok();
            }
            up.headers = ratelimit::cap_retry_after(&up.headers, somewhere);
        }
    }
    Ok(up)
}

/// Identifies the build a proxy is running. The version alone is too coarse - a
/// day's worth of fixes can land under one version - so the binary's own mtime
/// stands in for "this exact build".
pub fn build_id() -> String {
    let stamp = std::env::current_exe()
        .and_then(std::fs::metadata)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs())
        .unwrap_or(0);
    format!("{}-{stamp}", env!("CARGO_PKG_VERSION"))
}

/// Where a running proxy records the account actually serving turns. The default
/// pointer says which account was CHOSEN; after a rotation the proxy may be
/// serving a different one, and a marker that shows the choice rather than the
/// reality reads as "active" next to an account that cannot serve at all.
///
/// One file per tool. A single shared one meant the Codex proxy's choice
/// overwrote the Claude proxy's, so the Claude dashboard read a Codex account as
/// the one serving its turns - and no Claude row matched it.
fn serving_file_for(paths: &Paths, tool: &str) -> std::path::PathBuf {
    match tool {
        "codex" => paths.store_dir().join("proxy-serving-codex"),
        // Claude's keeps the name it has always had, so a proxy already running
        // is not orphaned by an upgrade.
        _ => paths.store_dir().join("proxy-serving"),
    }
}

/// The account a running proxy is serving turns from, if one is running.
pub fn serving_account(paths: &Paths) -> Option<String> {
    serving_account_for(paths, "claude-code")
}

/// The same, for one tool's proxy.
/// Where the proxy records the account it LAST served. Its timestamp is how a
/// caller tells whether the proxy has acted since the user asked for one.
pub fn serving_record_file(paths: &Paths, tool: &str) -> std::path::PathBuf {
    serving_file_for(paths, tool)
}

pub fn serving_account_for(paths: &Paths, tool: &str) -> Option<String> {
    running_proxy_for(paths, tool)?;
    std::fs::read_to_string(serving_file_for(paths, tool))
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

/// The client's own credential paid for this turn, not any account swapdex
/// holds. Erase the mark rather than leave the last name standing: "nobody"
/// is the true answer and every screen already renders it as no payer.
fn note_client_serving(paths: &Paths, tool: &str) {
    let _ = std::fs::remove_file(serving_file_for(paths, tool));
}

/// Record who is serving, but only on a change - this runs per request.
fn note_serving_for(paths: &Paths, tool: &str, name: &str) {
    let f = serving_file_for(paths, tool);
    if std::fs::read_to_string(&f).is_ok_and(|c| c.trim() == name) {
        return;
    }
    // Read from the request path by `ls`, the status bar and the shim. A plain
    // write truncates first, so a reader landing in that window sees an empty
    // file or half a name and reports the wrong payer. The next request rewrites
    // it if this one fails, so a discarded error is self-correcting.
    let _ = crate::atomic::write_secret(&f, name.as_bytes());
}

/// The port a running `swapdex proxy` announced, or `None` when none is up. The
/// marker holds "<pid> <port>"; a stale marker (hard-killed proxy) is ignored by
/// checking the pid, the same rule the shim applies.
pub fn running_port(paths: &Paths) -> Option<u16> {
    running_proxy(paths).map(|(_, port, _)| port)
}

/// The running proxy as (pid, port, build). `None` when none is up.
pub fn running_proxy(paths: &Paths) -> Option<(i32, u16, String)> {
    running_proxy_for(paths, "claude-code")
}

/// The same, for one tool's proxy.
/// Seconds east of UTC where this machine is, so a reset time reads as the
/// clock on the wall rather than as UTC. Zero if the platform will not say.
pub fn tz_offset() -> i64 {
    let t = now_secs() as libc::time_t;
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    if unsafe { libc::localtime_r(&t, &mut tm) }.is_null() {
        return 0;
    }
    tm.tm_gmtoff as i64
}

/// The port is taken. If the holder is ANOTHER swapdex proxy for this same tool,
/// take it over.
///
/// A shim-started proxy that outlives its shell is reparented to launchd and
/// keeps the port. The supervised agent then cannot bind, exits 1, and KeepAlive
/// restarts it into that same failure for as long as the machine is on - 166
/// times on a real machine before anyone looked. Only a swapdex proxy serving
/// THIS tool is displaced: anything else on the port is somebody else's and is
/// left alone, so the error still surfaces.
fn take_the_port(paths: &Paths, tool: &str, port: u16) -> Option<tiny_http::Server> {
    let (pid, held, _) = running_proxy_for(paths, tool)?;
    if held != port {
        return None;
    }
    println!("  another swapdex {tool} proxy (pid {pid}) holds {port} - taking it over");
    std::io::stdout().flush().ok();
    unsafe { libc::kill(pid, libc::SIGTERM) };
    // Give it a moment to release, then the one retry. Looping here would just
    // be the supervisor's restart loop moved inside the process.
    for _ in 0..40 {
        std::thread::sleep(std::time::Duration::from_millis(50));
        if let Ok(s) = tiny_http::Server::http(("127.0.0.1", port)) {
            return Some(s);
        }
    }
    None
}

pub fn running_proxy_for(paths: &Paths, tool: &str) -> Option<(i32, u16, String)> {
    let raw = std::fs::read_to_string(crate::shim::proxy_marker_for(paths, tool)).ok()?;
    let mut it = raw.split_whitespace();
    let pid: i32 = it.next()?.parse().ok()?;
    let port: u16 = it.next()?.parse().ok()?;
    // A marker written by an older build carries no build id; treat that as
    // "unknown", which is not the current one and so gets replaced.
    let build = it.next().unwrap_or("").to_string();
    // Signal 0 tests for existence without touching the process.
    (unsafe { libc::kill(pid, 0) } == 0).then_some((pid, port, build))
}

#[cfg(test)]
mod login_present_tests {
    use super::*;
    use crate::proxy::creds::TokenUnavailable;

    /// The distinction the row builders kept losing. It only shows itself on
    /// macOS - elsewhere there is no Keychain to be locked - so it is pinned
    /// here, where the value can be constructed on any platform, rather than
    /// left to a test that silently proves nothing off a Mac.
    #[test]
    fn a_locked_keychain_is_a_signed_in_account() {
        assert!(
            login_present(Err(TokenUnavailable::KeychainLocked)),
            "a Keychain that will not open is not an account nobody signed into"
        );
        assert!(
            !login_present(Err(TokenUnavailable::NoLogin)),
            "nothing to read is a missing login"
        );
        assert!(login_present(Ok(crate::secret::Secret::new(b"t".to_vec()))));
    }
}

#[cfg(test)]
mod seed_tests {
    use super::*;

    fn entry(at: i64, five_h: f64) -> crate::quota_cache::Entry {
        crate::quota_cache::Entry {
            five_h: Some(five_h),
            five_h_reset: Some(at + 3600),
            seven_d: Some(10.0),
            seven_d_reset: Some(at + 86400),
            at,
            on_credits: false,
            refused: None,
        }
    }

    /// A restart used to throw every reading away, so each one asked the usage
    /// endpoint again the moment the proxy came up - even for an account read
    /// seconds earlier. Several accounts arriving at once is exactly the burst
    /// that endpoint throttles, and three restarts in an afternoon put every
    /// account on this machine into "usage endpoint throttled" at once.
    #[test]
    fn a_reading_taken_moments_ago_is_not_due_again_after_a_restart() {
        let now = 1_800_000_000;
        let mut cache = crate::quota_cache::Cache::new();
        cache.insert("bsgong".into(), entry(now - 5, 87.0));
        let seeded = seed_from_cache(&cache, now);
        let m = seeded.get("bsgong").expect("carried over");
        assert_eq!(m.five_h, Some(87.0));
        assert_eq!(m.seven_d, Some(10.0));
        // 87% used leaves 13% - measure_after says 60s for that - and only 5
        // seconds have passed, so it is not due.
        let taken = m.taken.expect("a restored reading knows its age");
        let due = taken.elapsed() >= pick::measure_after(pick::headroom(m.five_h, m.seven_d));
        assert!(
            !due,
            "an account read 5s ago must not be asked again at once"
        );
    }

    /// Old enough to be worth asking again: the cache must not freeze a reading
    /// in place either.
    #[test]
    fn a_stale_reading_is_still_due() {
        let now = 1_800_000_000;
        let mut cache = crate::quota_cache::Cache::new();
        cache.insert("rnd".into(), entry(now - 3600, 87.0));
        let seeded = seed_from_cache(&cache, now);
        let m = seeded.get("rnd").unwrap();
        let taken = m.taken.expect("age");
        assert!(
            taken.elapsed() >= pick::measure_after(pick::headroom(m.five_h, m.seven_d)),
            "an hour-old reading is due"
        );
    }

    /// A reading from the future, or a clock that jumped, must not become an
    /// Instant in the future - that would make the account never due again.
    #[test]
    fn a_reading_stamped_ahead_of_now_is_treated_as_just_taken() {
        let now = 1_800_000_000;
        let mut cache = crate::quota_cache::Cache::new();
        cache.insert("x".into(), entry(now + 999, 50.0));
        let m = seed_from_cache(&cache, now);
        let taken = m.get("x").unwrap().taken.expect("age");
        assert!(
            taken.elapsed() < std::time::Duration::from_secs(2),
            "clamped to now"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::is_auth_exchange;

    // The exemption skips token injection, so it must match an authentication
    // path and nothing else - ordinary traffic that slipped through it would be
    // served by whatever credential the client happened to send.
    #[test]
    fn only_authentication_paths_are_exempt() {
        for p in [
            "/v1/oauth/token",
            "/oauth/authorize",
            "/v1/oauth/revoke?x=1",
            "/v1/OAuth/token",
            "/login",
            "/api/auth/callback",
        ] {
            assert!(is_auth_exchange(p), "should be exempt: {p}");
        }
        for p in [
            "/v1/messages",
            "/v1/messages?beta=true",
            "/api/hello",
            "/v1/responses",
            // A segment merely CONTAINING the word is not an auth path: matching
            // on substrings would exempt real traffic.
            "/v1/authors",
            "/v1/oauthorization-notes",
        ] {
            assert!(!is_auth_exchange(p), "must NOT be exempt: {p}");
        }
    }
}

#[cfg(test)]
mod live_settings_tests {
    use super::*;

    /// `swapdex auto on` has to reach the proxy that is already running. The
    /// settings were read ONCE at startup, so changing one did nothing until
    /// somebody restarted the proxy - and nothing tells a user to, or does it for
    /// them. The pointers deciding who serves are read per request already; a
    /// setting is no different.
    ///
    /// An explicit `--auto` / `--no-auto` on the command line still wins: that is
    /// a decision about THIS run, not a default to be overridden from elsewhere.
    #[test]
    fn a_flag_wins_and_otherwise_the_setting_is_read_now() {
        assert!(auto_now(Some(true), false), "--auto stands");
        assert!(!auto_now(Some(false), true), "--no-auto stands");
        assert!(auto_now(None, true), "no flag: follow the setting");
        assert!(!auto_now(None, false));
    }
}

#[cfg(test)]
mod response_header_tests {
    use super::*;

    /// The one that broke real sessions. ureq decodes gzip on the way in, so
    /// the body handed to the client is plain; echoing the label tells it to
    /// gunzip text that is already text, and the stream dies partway through.
    #[test]
    fn the_encoding_label_does_not_outlive_the_encoding() {
        assert!(skip_response_header("content-encoding"));
        assert!(skip_response_header("Content-Encoding"));
        // The length describes bytes that no longer exist either.
        assert!(skip_response_header("content-length"));
    }

    /// Everything the client needs is still passed through - dropping too much
    /// would trade one broken stream for another.
    #[test]
    fn the_headers_the_client_needs_are_kept() {
        for keep in [
            "content-type",
            "anthropic-ratelimit-unified-5h-status",
            "x-codex-primary-used-percent",
            "retry-after",
            "request-id",
            "cache-control",
        ] {
            assert!(!skip_response_header(keep), "{keep} must reach the client");
        }
    }

    /// A request may legitimately carry an encoded body, and this proxy passes
    /// that body through untouched - so the upward filter must NOT drop the
    /// label the way the downward one does.
    #[test]
    fn a_request_body_keeps_its_own_encoding() {
        assert!(!skip_header("content-encoding"));
    }
}

#[cfg(test)]
mod refusal_recording_tests {
    use super::*;

    /// A 429 that is about to be RETRIED on the same account is not that
    /// account refusing to serve.
    ///
    /// `note_outcome` is called inside the retry loop, so a momentary throttle
    /// stamped the account as refusing before the retry had even run. When a
    /// retry then succeeded the stamp was overwritten, but when the round ended
    /// any other way the account stayed sidelined for being briefly slow -
    /// exactly the "count refusal rounds, not responses" mistake.
    ///
    /// Only the verdict that ends the round is about the account.
    #[test]
    fn a_throttle_that_will_be_retried_is_not_a_refusal() {
        // Mid-round: the proxy is going to try this same account again.
        assert!(!records_refusal(429, true));
        // Final: retries are spent, or the response was never retryable.
        assert!(records_refusal(429, false));
        // Entitlement refusals are never retried on the same account, so they
        // are always the end of the round.
        assert!(records_refusal(403, false));
        assert!(records_refusal(401, false));
        // A success is a success whenever it lands.
        assert!(!records_refusal(200, false));
    }
}

#[cfg(test)]
mod overloaded_tests {
    use super::*;

    /// 529 is the SERVER being overloaded, not this account being spent.
    ///
    /// It was passed straight through, so the turn died and the user saw a
    /// connection drop, 54 of them in one day's log. Waiting briefly usually
    /// clears it, and switching accounts cannot: every account talks to the
    /// same overloaded server, so rotating would only drop the prompt cache.
    #[test]
    fn an_overloaded_server_is_retried_on_the_same_account() {
        assert_eq!(
            overload_retry(529, 0),
            Some(std::time::Duration::from_secs(1))
        );
        assert_eq!(
            overload_retry(529, 1),
            Some(std::time::Duration::from_secs(2))
        );
        assert_eq!(
            overload_retry(529, 2),
            Some(std::time::Duration::from_secs(4))
        );
        // The budget has to outlast a REAL overload, not a textbook one. This
        // machine's logs show a 529 spell running for a minute at a time -
        // eighteen in one minute - so a 7-second budget surfaced the error to
        // the user while the wave was still passing.
        assert_eq!(
            overload_retry(529, 3),
            Some(std::time::Duration::from_secs(8))
        );
        assert_eq!(
            overload_retry(529, 4),
            Some(std::time::Duration::from_secs(15))
        );
        assert_eq!(
            overload_retry(529, 5),
            Some(std::time::Duration::from_secs(15))
        );
        // Still bounded: a server that stays down must surface, not loop forever.
        assert_eq!(overload_retry(529, 8), None);
        // Everything else is somebody else's decision.
        assert_eq!(overload_retry(200, 0), None);
        assert_eq!(overload_retry(429, 0), None);
        assert_eq!(overload_retry(500, 0), None);
    }

    /// And it is never mistaken for the account refusing: sidelining an account
    /// over the server's own load would take a healthy login out of rotation.
    #[test]
    fn an_overloaded_server_does_not_sideline_the_account() {
        assert!(!ratelimit::account_cannot_serve(529));
    }
}

#[cfg(test)]
mod measure_without_auto_tests {
    use super::*;

    /// Reading the quota and rotating on it are different jobs.
    ///
    /// The refresh sat inside `if auto`, so with rotation off nothing ever
    /// re-read the windows: the cache aged, its readings expired at their reset
    /// times, and the usage simply vanished from every screen. Turning
    /// rotation off should cost the rotation, not the numbers.
    #[test]
    fn the_quota_is_refreshed_even_with_rotation_off() {
        // Rotation on: measured, as before.
        assert!(should_measure(true, "claude-code"));
        // Rotation off: still measured - the display needs it.
        assert!(should_measure(false, "claude-code"));
        // Codex has no zero-spend usage endpoint of this kind, so it is never
        // measured this way whatever the rotation setting says.
        assert!(!should_measure(true, "codex"));
        assert!(!should_measure(false, "codex"));
    }
}

#[cfg(test)]
mod transport_retry_tests {
    use super::*;

    /// A dropped connection deserves the same patience as a 529.
    ///
    /// `forward` returned its error straight up, so an ECONNRESET mid-flight
    /// reached the user as "API error" - while the identical condition
    /// expressed as a 529 status was retried politely. The next attempt
    /// usually succeeds.
    #[test]
    fn a_dropped_connection_is_retried_briefly() {
        assert_eq!(
            transport_retry(0),
            Some(std::time::Duration::from_millis(250))
        );
        assert_eq!(
            transport_retry(1),
            Some(std::time::Duration::from_millis(500))
        );
        assert_eq!(
            transport_retry(3),
            Some(std::time::Duration::from_millis(2000))
        );
        // Bounded: a network that stays down has to surface.
        assert_eq!(transport_retry(4), None);
    }
}

#[cfg(test)]
mod accept_failure_tests {
    use super::*;

    /// A connection that dies before the proxy can read it must be RECORDED.
    ///
    /// `server.recv()` errors were swallowed with `continue`, so a client whose
    /// connection dropped mid-request left no trace at all - the proxy's log
    /// showed only successes while the user watched "API error" repeatedly, and
    /// there was nothing anywhere to tell the two apart. A failure nobody can
    /// see is the hardest kind to fix.
    ///
    /// Rate-limited: a broken client can fail hundreds of times a second, and a
    /// log that floods is as unreadable as one that says nothing.
    #[test]
    fn a_dropped_client_connection_is_reported_but_not_flooded() {
        let mut g = AcceptFailures::default();
        // First one speaks.
        assert!(g.should_report(1_000));
        // The flood behind it does not.
        assert!(!g.should_report(1_000));
        assert!(!g.should_report(1_005));
        // Once the window passes, it speaks again - with the count it swallowed.
        assert!(g.should_report(1_000 + ACCEPT_REPORT_SECS));
        assert_eq!(g.take_suppressed(), 2);
        // And the counter resets after being reported.
        assert_eq!(g.take_suppressed(), 0);
    }
}

#[cfg(test)]
mod poison_tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    /// One dead thread must not disable the proxy for every request after it.
    ///
    /// A thread that panics while holding a Mutex poisons it, and `.lock()`
    /// returns Err from then on. With `.unwrap()` on the other side, every
    /// later request panics too - so the proxy keeps listening, systemctl
    /// keeps saying active, and every request fails. That failure is invisible
    /// from the outside, which is what makes it dangerous.
    #[test]
    fn a_thread_that_dies_holding_a_lock_does_not_disable_it_for_everyone_else() {
        let m = Arc::new(Mutex::new(vec!["rnd".to_string()]));
        let other = Arc::clone(&m);

        let prev = std::panic::take_hook();
        std::panic::set_hook(Box::new(|_| {}));
        let _ = std::thread::spawn(move || {
            let _g = other.lock().unwrap();
            panic!("a background thread died while holding the lock");
        })
        .join();
        std::panic::set_hook(prev);

        assert!(m.lock().is_err(), "the lock really is poisoned");
        assert_eq!(m.held().len(), 1, "the proxy must still be able to read it");
    }
}

#[cfg(test)]
mod pointer_tests {
    use super::*;
    use std::os::unix::fs::MetadataExt;

    /// The serving pointer is read while it is being written.
    ///
    /// This file records who pays and is rewritten from the request path; `ls`,
    /// the status bar and the shim all read it, constantly. A plain write
    /// truncates first, so a reader landing in that window sees an empty file
    /// or half a name and reports the wrong payer. Replacing the file by rename
    /// leaves no such window - and the inode changes because the file was
    /// replaced rather than overwritten in place.
    #[test]
    fn the_serving_pointer_is_replaced_rather_than_truncated() {
        let root = tempfile::tempdir().unwrap();
        let paths = crate::paths::Paths::rooted(root.path());
        let f = serving_file_for(&paths, "claude-code");
        std::fs::create_dir_all(f.parent().unwrap()).unwrap();

        note_serving_for(&paths, "claude-code", "alpha");
        let before = std::fs::metadata(&f).unwrap().ino();
        note_serving_for(&paths, "claude-code", "beta");
        let after = std::fs::metadata(&f).unwrap().ino();

        assert_ne!(
            before, after,
            "the pointer was overwritten in place - a reader can see half a name"
        );
    }
}

#[cfg(test)]
mod write_back_tests {
    use super::*;

    /// A round that read nothing must not restamp the old number as current.
    #[test]
    fn only_a_landed_reading_counts_as_read() {
        assert!(!reading_landed(&crate::quota::Fetch::Unauthorized));
        assert!(!reading_landed(&crate::quota::Fetch::Unexpected(
            429,
            "rate limited".into()
        )));
        assert!(!reading_landed(&crate::quota::Fetch::Offline(
            "could not resolve host".into()
        )));
        let q = crate::quota::parse(
            r#"{"five_hour":{"utilization":0.04,"resets_at":"2026-08-26T12:00:00Z"}}"#,
        );
        if let Some(q) = q {
            assert!(reading_landed(&crate::quota::Fetch::Ok(q)));
        }
    }
}
