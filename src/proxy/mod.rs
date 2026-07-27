//! Proxy mode: a loopback HTTP server that forwards Claude Code's API traffic
//! upstream, choosing the account per request so a running conversation can
//! change accounts. Credentials are read from slots and never copied, and
//! neither prompt content nor any token value is ever logged.

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
}

pub struct Opts {
    pub port: u16,
    pub account: Option<String>,
    /// Continue on another account when the current one is spent (Task 6).
    pub auto: bool,
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
fn pick_slot(paths: &Paths, opts: &Opts, sh: &Shared) -> Result<crate::slots::SlotRecord> {
    let slots = crate::slots::Slots::open(paths)?;
    if let Some(name) = &opts.account {
        return slots
            .get(name)
            .ok_or_else(|| anyhow!("no account slot named '{name}' - `swapdex slots` lists them"));
    }
    let list = slots.list();
    let pointer = slots.default_dir();
    let rotated = sh.rotated.lock().unwrap().clone();
    sh.chooser
        .lock()
        .unwrap()
        .choose(pointer.as_deref(), rotated.as_deref(), &list)
        .ok_or_else(|| anyhow!("no account slots yet - `swapdex run <name>` creates one"))
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
    let marker = crate::shim::proxy_marker(paths);
    let _ = std::fs::create_dir_all(paths.store_dir());
    let announced = std::fs::write(&marker, format!("{} {port}\n", std::process::id())).is_ok();
    if announced {
        // Ctrl-C is the normal way to stop a foreground proxy, so clean up there
        // too - otherwise the shim would keep pointing at a dead port until the
        // pid check catches it.
        let m = marker.clone();
        let _ = ctrl_c_cleanup(move || {
            let _ = std::fs::remove_file(&m);
        });
    }
    println!("swapdex proxy listening on http://127.0.0.1:{port}");
    if announced && crate::shim::shim_path(paths).exists() {
        println!("  a plain `claude` now goes through it (the shim picks it up)");
    } else {
        println!("  point Claude at it:  export ANTHROPIC_BASE_URL=http://127.0.0.1:{port}");
    }
    if opts.auto {
        println!("  --auto: a spent account hands the session to another one");
    }
    std::io::stdout().flush().ok();

    let server = Arc::new(server);
    let sh = Arc::new(Shared {
        agent: upstream::agent(),
        base: upstream::base_url(),
        quota: Mutex::new(HashMap::new()),
        chooser: Mutex::new(pick::Chooser::default()),
        rotated: Mutex::new(None),
        unusable: Mutex::new(std::collections::HashSet::new()),
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
            auto: opts.auto,
        };
        std::thread::spawn(move || {
            if let Err(e) = handle(rq, &paths, &opts, &sh) {
                eprintln!("swapdex proxy: {e:#}");
            }
        });
    }
}

/// Hand the session to another account. `spent` distinguishes "out of quota"
/// from "login refused" so the line the user reads says which happened. An
/// account is skipped when it is out of quota OR its login was refused.
fn rotate_away_from(current: &str, paths: &Paths, sh: &Shared, spent: bool) {
    let slots = crate::slots::Slots::open(paths)
        .map(|s| s.list())
        .unwrap_or_default();
    let mut st = sh.quota.lock().unwrap().clone();
    for name in sh.unusable.lock().unwrap().iter() {
        st.entry(name.clone()).or_default().rejected = true;
    }
    let why = if spent { "is spent" } else { "cannot sign in" };
    match pick::rotate_target(current, &slots, &st) {
        Some(next) => {
            println!("{current} {why} - continuing on {next}");
            *sh.rotated.lock().unwrap() = Some(next);
        }
        None => println!("{current} {why} and no other account is usable"),
    }
}

fn handle(mut rq: tiny_http::Request, paths: &Paths, opts: &Opts, sh: &Shared) -> Result<()> {
    let slot = pick_slot(paths, opts, sh)?;
    let token = creds::slot_token_detail(&slot.config_dir)
        .map_err(|why| anyhow!("{}", why.remedy(&slot.name)))?;

    let mut headers: Vec<(String, String)> = rq
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
    headers.push((
        "authorization".into(),
        format!("Bearer {}", String::from_utf8_lossy(token.expose())),
    ));

    let url = format!("{}{}", sh.base, rq.url());
    let method = rq.method().as_str().to_string();
    let path = rq.url().to_string();
    let mut body = Vec::new();
    rq.as_reader().read_to_end(&mut body)?;

    // Keep the account identity in the body consistent with the token serving
    // this turn: after a switch or rotation the client still names the account it
    // started with. Only UUIDs of swapdex-managed accounts are substituted, and
    // the body is forwarded byte-for-byte when there is nothing to align.
    if let Some(serving) = creds::slot_account_uuid(&slot.config_dir) {
        let known: Vec<String> = crate::slots::Slots::open(paths)
            .map(|s| {
                s.list()
                    .iter()
                    .filter_map(|r| creds::slot_account_uuid(&r.config_dir))
                    .collect()
            })
            .unwrap_or_default();
        if let Some(aligned) = identity::align_account(&body, &known, &serving) {
            body = aligned;
        }
    }

    // A 429 is two different events wearing one status. A THROTTLE ("slow down",
    // x-should-retry) is fixed by waiting and retrying this same account - the
    // client would otherwise just see the failure. Real exhaustion is not
    // retried here; it falls through so rotation can handle it.
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
                // Drop this response (and its body) before sleeping.
                drop(up);
                attempt += 1;
                std::thread::sleep(wait);
            }
            ratelimit::Throttle::Exhausted => break up,
        }
    };
    // Log the account and the outcome only - never a body, never a token. The
    // quota state rides along on responses the user was making anyway.
    let quota = ratelimit::from_headers(&up.headers);
    match &quota {
        Some(q) if q.rejected => println!("{} {path} -> {} SPENT", slot.name, up.status),
        _ => println!("{} {path} -> {}", slot.name, up.status),
    }
    std::io::stdout().flush().ok();
    let mut spent = false;
    if let Some(q) = quota {
        spent = q.rejected;
        sh.quota.lock().unwrap().insert(slot.name.clone(), q);
    }
    // A 401 is not a quota problem: that account's login needs re-establishing
    // (its access token lapsed and swapdex does not mint tokens yet). Say the one
    // thing that fixes it instead of leaving a bare 401, and take the account out
    // of rotation for this run so it is not tried again and again.
    if up.status == 401 {
        println!(
            "{}: login no longer accepted - run `swapdex run {}` once to sign it in again",
            slot.name, slot.name
        );
        sh.unusable.lock().unwrap().insert(slot.name.clone());
    }
    // Turn-boundary rotation: this response is already complete, so switching now
    // cannot sever an answer - the NEXT turn carries the new account.
    if opts.auto && (spent || up.status == 401) {
        rotate_away_from(&slot.name, paths, sh, spent);
    }
    std::io::stdout().flush().ok();

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
