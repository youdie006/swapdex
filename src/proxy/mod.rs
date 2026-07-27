//! Proxy mode: a loopback HTTP server that forwards Claude Code's API traffic
//! upstream, choosing the account per request so a running conversation can
//! change accounts. Credentials are read from slots and never copied, and
//! neither prompt content nor any token value is ever logged.

pub mod creds;
pub mod upstream;

use crate::paths::Paths;
use anyhow::{anyhow, Result};
use std::io::Write;
use std::sync::Arc;

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

/// The slot the next request should use: an explicit `--account`, else the
/// `active-claude` pointer resolved to a slot, else the only slot there is.
fn pick_slot(paths: &Paths, opts: &Opts) -> Result<crate::slots::SlotRecord> {
    let slots = crate::slots::Slots::open(paths)?;
    if let Some(name) = &opts.account {
        return slots
            .get(name)
            .ok_or_else(|| anyhow!("no account slot named '{name}' - `swapdex slots` lists them"));
    }
    let list = slots.list();
    if let Some(dir) = slots.default_dir() {
        if let Some(r) = list.iter().find(|r| r.config_dir == dir) {
            return Ok(r.clone());
        }
    }
    list.into_iter()
        .next()
        .ok_or_else(|| anyhow!("no account slots yet - `swapdex run <name>` creates one"))
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
    println!("swapdex proxy listening on http://127.0.0.1:{port}");
    println!("  point Claude at it:  export ANTHROPIC_BASE_URL=http://127.0.0.1:{port}");
    std::io::stdout().flush().ok();

    let server = Arc::new(server);
    let agent = Arc::new(upstream::agent());
    let base = upstream::base_url();
    loop {
        let rq = match server.recv() {
            Ok(r) => r,
            Err(_) => continue,
        };
        let paths = paths.clone();
        let agent = agent.clone();
        let base = base.clone();
        let opts = Opts {
            port,
            account: opts.account.clone(),
            auto: opts.auto,
        };
        std::thread::spawn(move || {
            if let Err(e) = handle(rq, &paths, &opts, &agent, &base) {
                eprintln!("swapdex proxy: {e:#}");
            }
        });
    }
}

fn handle(
    mut rq: tiny_http::Request,
    paths: &Paths,
    opts: &Opts,
    agent: &ureq::Agent,
    base: &str,
) -> Result<()> {
    let slot = pick_slot(paths, opts)?;
    let token = creds::slot_token(&slot.config_dir).ok_or_else(|| {
        anyhow!(
            "account '{}' has no usable login - `swapdex run {}` once signs it in",
            slot.name,
            slot.name
        )
    })?;

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

    let url = format!("{base}{}", rq.url());
    let method = rq.method().as_str().to_string();
    let path = rq.url().to_string();
    let mut body = Vec::new();
    rq.as_reader().read_to_end(&mut body)?;

    let up = upstream::forward(agent, &method, &url, &headers, &body)?;
    // Log the account and the outcome only - never a body, never a token.
    println!("{} {path} -> {}", slot.name, up.status);
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
