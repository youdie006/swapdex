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

/// Everything the request threads share: the upstream client, where upstream is,
/// the per-account quota state, and the account-choice state. One struct so a
/// request handler keeps a small signature as the proxy grows.
/// One account's measured utilization: the 5h and 7d percentages, either of which
/// may be unmeasured.
type Utilization = (Option<f64>, Option<f64>);

/// Measured utilization per account, with when the reading was taken.
type Measured = (Option<std::time::Instant>, HashMap<String, Utilization>);

struct Shared {
    agent: ureq::Agent,
    base: String,
    /// Last seen quota state per account name.
    quota: Mutex<HashMap<String, ratelimit::Quota>>,
    chooser: Mutex<pick::Chooser>,
    /// The proxy's own current account after a rotation, if any.
    rotated: Mutex<Option<String>>,
    /// Accounts the upstream refused outright (401): not a quota problem, so kept
    /// apart from quota state, but equally out of rotation for this run.
    unusable: Mutex<std::collections::HashSet<String>>,
    /// Measured utilization per account (5h, 7d percentages) from the zero-spend
    /// usage endpoint, with when it was read. Only used when a threshold is set.
    measured: Mutex<Measured>,
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
    /// Continue on another account when the current one is spent.
    pub auto: bool,
    /// Step off an account once a window reaches this fraction (0.98 = 98%),
    /// BEFORE it refuses a turn. `None` waits for the refusal instead, which costs
    /// one failed turn per wall. Needs one usage read per account, so it is opt-in.
    pub threshold: Option<f64>,
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
    let rotated = sh.rotated.lock().unwrap().clone();
    let chosen = sh
        .chooser
        .lock()
        .unwrap()
        .choose(pointer.as_deref(), rotated.as_deref(), &list)
        .ok_or_else(|| anyhow!("no account slots yet - `swapdex run <name>` creates one"))?;
    // With --auto, an account already known to be out of quota should not serve
    // the next turn: the previous response said a window was spent, so start
    // elsewhere instead of walking into the wall. The turn that OBSERVED this was
    // still served by that account (rotating mid-turn would drop the prompt cache
    // for nothing), which is why the check belongs here and not there.
    if opts.auto {
        // Stepping off BEFORE the wall needs a reading, and the only zero-spend
        // reading that exists is Anthropic's usage endpoint. Codex has none, so
        // its accounts are moved when one actually refuses a turn - never by
        // asking one API about another's account.
        if let Some(t) = opts.threshold.filter(|_| opts.tool != "codex") {
            refresh_measured(&list, sh);
            let full = sh
                .measured
                .lock()
                .unwrap()
                .1
                .get(&chosen.name)
                .is_some_and(|(a, b)| pick::over_threshold(*a, *b, t));
            // A move made moments ago stands: without this, two accounts either
            // side of the line trade the session back and forth.
            let cooling = sh
                .last_preempt
                .lock()
                .unwrap()
                .is_some_and(|t| t.elapsed() < PREEMPT_COOLDOWN);
            if full && !cooling {
                match usable_under_threshold(paths, sh, &chosen.name, t) {
                    Some(better) => {
                        println!(
                            "{} is near its limit - starting this turn on {}",
                            chosen.name, better.name
                        );
                        std::io::stdout().flush().ok();
                        *sh.rotated.lock().unwrap() = Some(better.name.clone());
                        *sh.last_preempt.lock().unwrap() = Some(std::time::Instant::now());
                        return Ok(better);
                    }
                    // Staying put on a full account is the right call when every
                    // other one is full too - but silence here is indistinguishable
                    // from the threshold not working at all.
                    None => {
                        println!(
                            "{} is past the threshold, and no other account is below it - \
                             staying here",
                            chosen.name
                        );
                        std::io::stdout().flush().ok();
                    }
                }
            }
        }
        let known_spent = sh
            .quota
            .lock()
            .unwrap()
            .get(&chosen.name)
            .is_some_and(|q| q.rejected)
            || sh.unusable.lock().unwrap().contains(&chosen.name)
            // A lapsed token cannot serve and cannot be refreshed from here, so
            // treat it the same as spent when choosing where to start.
            || creds::slot_token_expired(&chosen.config_dir, now_ms());
        if known_spent {
            if let Some(better) = next_account(paths, sh, std::slice::from_ref(&chosen.name)) {
                return Ok(better);
            }
        }
    }
    Ok(chosen)
}

/// How long a utilization reading is trusted before being taken again. Long
/// enough that the proxy is not a stream of requests, short enough that a fast
/// burn is noticed before it hits the wall.
const MEASURE_EVERY: std::time::Duration = std::time::Duration::from_secs(120);

/// How long a pre-emptive move stands before another can happen. Long enough that
/// two accounts near the line cannot trade the session between them, short enough
/// that a genuine second wall is still stepped over.
const PREEMPT_COOLDOWN: std::time::Duration = std::time::Duration::from_secs(300);

/// Read each account's utilization from the zero-spend usage endpoint, at most
/// once per `MEASURE_EVERY`. This is the same read `swapdex quota` performs, with
/// each account's own token; it spends no message quota.
fn refresh_measured(slots: &[crate::slots::SlotRecord], sh: &Arc<Shared>) {
    let first = {
        let mut m = sh.measured.lock().unwrap();
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
        measure_now(slots, sh);
        return;
    }
    let slots: Vec<crate::slots::SlotRecord> = slots.to_vec();
    let sh = Arc::clone(sh);
    std::thread::spawn(move || measure_now(&slots, &sh));
}

/// The read itself. Records when it finished, not when it began, so a slow read
/// is not immediately due again.
fn measure_now(slots: &[crate::slots::SlotRecord], sh: &Shared) {
    let mut out = HashMap::new();
    for r in slots {
        let Some(tok) = creds::slot_token(&r.config_dir) else {
            continue;
        };
        let token = String::from_utf8_lossy(tok.expose()).to_string();
        if !crate::quota::token_usable(&token) {
            continue;
        }
        crate::quota::pace_between_accounts();
        if let crate::quota::Fetch::Ok(q) = crate::quota::fetch_with_retry(&token) {
            out.insert(
                r.name.clone(),
                (
                    q.five_hour.map(|w| w.used_pct),
                    q.seven_day.map(|w| w.used_pct),
                ),
            );
        }
    }
    // Say what came back. A threshold that never fires because nothing could be
    // measured is indistinguishable from one that is working, and this read is
    // the only thing standing between the two.
    if out.is_empty() {
        println!("  (no account's usage could be read - the threshold cannot apply)");
    } else {
        let mut parts: Vec<String> = out
            .iter()
            .map(|(n, (a, b))| {
                let worst = [*a, *b].into_iter().flatten().fold(f64::NAN, f64::max);
                if worst.is_finite() {
                    format!("{n} {worst:.0}%")
                } else {
                    format!("{n} ?")
                }
            })
            .collect();
        parts.sort();
        println!("  usage: {}", parts.join(", "));
    }
    std::io::stdout().flush().ok();
    let mut m = sh.measured.lock().unwrap();
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
    let measured = sh.measured.lock().unwrap();
    let spent = sh.quota.lock().unwrap();
    let unusable = sh.unusable.lock().unwrap();
    // Most room first, so the session lands somewhere it can stay - picking the
    // first eligible account tends to land on one that is nearly full too.
    pick::by_headroom(
        &mut slots,
        |r| cfg.rank(&r.name),
        |r| {
            measured
                .1
                .get(&r.name)
                .and_then(|(a, b)| pick::headroom(*a, *b))
        },
    );
    slots.into_iter().find(|r| {
        r.name != current
            && !cfg.is_disabled(&r.name)
            && !unusable.contains(&r.name)
            && !spent.get(&r.name).is_some_and(|q| q.rejected)
            && !measured
                .1
                .get(&r.name)
                .is_some_and(|(a, b)| pick::over_threshold(*a, *b, threshold))
            && creds::slot_token(&r.config_dir).is_some()
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
    // Loopback only: this holds a live credential, so it must never be
    // reachable off the machine.
    let server = tiny_http::Server::http(("127.0.0.1", opts.port))
        .map_err(|e| anyhow!("cannot bind 127.0.0.1:{}: {e}", opts.port))?;
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
        let serving = serving_file(paths);
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
    match (opts.auto, opts.threshold.filter(|_| !is_codex)) {
        (true, Some(t)) => println!(
            "  auto: hands the session on at {:.0}% used, or when an account refuses",
            (t * 100.0).round()
        ),
        (true, None) => println!("  auto: hands the session on when an account refuses"),
        (false, _) => println!("  auto is off - `swapdex auto on` lets it move by itself"),
    }
    std::io::stdout().flush().ok();

    let server = Arc::new(server);
    let sh = Arc::new(Shared {
        agent: upstream::agent(),
        base: if opts.tool == "codex" {
            codex::base_url()
        } else {
            upstream::base_url()
        },
        quota: Mutex::new(HashMap::new()),
        chooser: Mutex::new(pick::Chooser::default()),
        rotated: Mutex::new(None),
        unusable: Mutex::new(std::collections::HashSet::new()),
        measured: Mutex::new((None, HashMap::new())),
        last_preempt: Mutex::new(None),
    });
    loop {
        let rq = match server.recv() {
            Ok(r) => r,
            Err(_) => continue,
        };
        let paths = paths.clone();
        let sh = sh.clone();
        let opts = Opts {
            port,
            account: opts.account.clone(),
            tool: opts.tool.clone(),
            auto: opts.auto,
            threshold: opts.threshold,
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
        let measured = sh.measured.lock().unwrap();
        pick::by_headroom(
            &mut slots,
            |r| cfg.rank(&r.name),
            |r| {
                measured
                    .1
                    .get(&r.name)
                    .and_then(|(a, b)| pick::headroom(*a, *b))
            },
        );
    }
    let spent = sh.quota.lock().unwrap();
    let unusable = sh.unusable.lock().unwrap();
    slots.into_iter().find(|r| {
        !tried.contains(&r.name)
            && !unusable.contains(&r.name)
            && !spent.get(&r.name).is_some_and(|q| q.rejected)
            // "Disabled" means do not pick this one FOR me; switching to it by
            // hand still works, which is why the check lives here and not in
            // pick_slot.
            && !cfg.is_disabled(&r.name)
            // Never offer a slot that was never signed into, or whose token has
            // already lapsed: either one just earns a 401, and nothing here can
            // refresh it. What a login LOOKS like differs per tool, and asking
            // the Claude question about a Codex slot answered "no login" for
            // every one of them - so a refused Codex turn had nowhere to go.
            && has_usable_login(tool, &r.config_dir)
    })
}

/// Can this slot serve a turn for `tool` right now?
fn has_usable_login(tool: &str, dir: &std::path::Path) -> bool {
    match tool {
        // Codex refreshes its own token inside its home and records no expiry
        // swapdex can read, so the question is only whether a login is there.
        "codex" => codex::slot_auth(dir).is_some(),
        _ => creds::slot_token(dir).is_some() && !creds::slot_token_expired(dir, now_ms()),
    }
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
        .filter(|(n, _)| !skip_header(n))
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
    note_serving(paths, &slot.name);
    let mut tried: Vec<String> = Vec::new();
    let up = loop {
        // An already-lapsed token earns a 401 and cannot be refreshed from here,
        // so treat it exactly like having no login: step aside rather than spend
        // the turn proving it.
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
                let mut headers = client_headers.clone();
                if let Some(a) = client_auth.clone() {
                    headers.push(("authorization".into(), a));
                }
                return upstream::forward(&sh.agent, &method, &url, &headers, &client_body);
            };
            let mut headers = client_headers.clone();
            codex::apply_auth(&mut headers, &auth);
            let up = upstream::forward(&sh.agent, &method, &url, &headers, &client_body)?;
            println!("  {} {} -> {} [{}]", method, path, up.status, slot.name);
            std::io::stdout().flush().ok();
            // A refusal means this account cannot serve the turn. With --auto,
            // hand the SAME turn to another account rather than returning the
            // failure - that is the whole point of continuing a session.
            if (up.status == 429 || up.status == 401) && opts.auto {
                tried.push(slot.name.clone());
                if up.status == 401 {
                    sh.unusable.lock().unwrap().insert(slot.name.clone());
                } else {
                    sh.quota
                        .lock()
                        .unwrap()
                        .entry(slot.name.clone())
                        .or_default()
                        .rejected = true;
                }
                if let Some(next) = next_account_for(paths, opts, sh, &tried) {
                    println!("  {} is out - continuing on {}", slot.name, next.name);
                    std::io::stdout().flush().ok();
                    *sh.rotated.lock().unwrap() = Some(next.name.clone());
                    note_serving(paths, &next.name);
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
        if let Some(serving) = creds::slot_account_uuid(&slot.config_dir) {
            if let Some(aligned) = identity::align_account(&body, &known_uuids, &serving) {
                body = aligned;
            }
        }

        // A 429 wears two meanings. A THROTTLE ("slow down", x-should-retry) is
        // fixed by waiting and retrying this same account.
        let mut attempt = 0u32;
        let up = loop {
            let up = upstream::forward(&sh.agent, &method, &url, &headers, &body)?;
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
                ratelimit::Throttle::Exhausted => break up,
            }
        };

        // Record what this response says about the account, and log it. A
        // rejected window on a SUCCESSFUL response is noted but not acted on:
        // the account is still serving, and rotating away would drop the
        // prompt cache (which is organization-scoped) for nothing.
        let quota = ratelimit::from_headers(&up.headers);
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
            sh.quota.lock().unwrap().insert(slot.name.clone(), q);
        }
        if up.status == 401 {
            println!(
                "{}: login no longer accepted - run `swapdex run {}` once to sign it in again",
                slot.name, slot.name
            );
            sh.unusable.lock().unwrap().insert(slot.name.clone());
        }
        if up.status != 429 && up.status != 401 {
            break up;
        }

        // The wall (or a dead login). Serve THIS turn on another account rather
        // than handing the client a failure - that is what "continue the session
        // elsewhere" has to mean. Without --auto, or with nothing left to try,
        // the client gets the real response.
        if up.status == 429 {
            sh.quota
                .lock()
                .unwrap()
                .entry(slot.name.clone())
                .or_default()
                .rejected = true;
        }
        if !opts.auto || opts.account.is_some() {
            break up;
        }
        tried.push(slot.name.clone());
        match next_account(paths, sh, &tried) {
            Some(next) => {
                println!(
                    "{} cannot serve this turn - retrying on {}",
                    slot.name, next.name
                );
                std::io::stdout().flush().ok();
                *sh.rotated.lock().unwrap() = Some(next.name.clone());
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
                let unusable = sh.unusable.lock().unwrap().clone();
                if !unusable.is_empty() && unusable.len() >= names.len().max(1) {
                    let first = names.first().cloned().unwrap_or_else(|| "<name>".into());
                    return Err(anyhow!(
                        "every account's login has expired. Run `swapdex run {first}` once \
                         (its own login refreshes there), then try again - swapdex does not \
                         mint tokens itself."
                    ));
                }
                println!("{}: no other account can serve this turn", slot.name);
                std::io::stdout().flush().ok();
                break up;
            }
        }
    };

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
pub fn serving_file(paths: &Paths) -> std::path::PathBuf {
    paths.store_dir().join("proxy-serving")
}

/// The account a running proxy is serving turns from, if one is running.
pub fn serving_account(paths: &Paths) -> Option<String> {
    running_port(paths)?;
    std::fs::read_to_string(serving_file(paths))
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

/// Record who is serving, but only on a change - this runs per request.
fn note_serving(paths: &Paths, name: &str) {
    let f = serving_file(paths);
    if std::fs::read_to_string(&f).is_ok_and(|c| c.trim() == name) {
        return;
    }
    let _ = std::fs::write(&f, name);
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
