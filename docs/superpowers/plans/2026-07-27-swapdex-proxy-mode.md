# swapdex Proxy Mode Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** `swapdex proxy` intercepts Claude Code's API traffic so the account can be chosen per request — the running conversation switches accounts on command, and optionally rotates itself when the active account hits its limit.

**Architecture:** A synchronous, thread-per-connection local HTTP server (`tiny_http`) that forwards to `api.anthropic.com` over `ureq`/rustls, injecting the chosen slot's token per request and streaming both directions. Account choice comes from the existing `active-claude` pointer (so `swapdex use` and the TUI already move it) with the proxy overriding only when quota forces a rotation. Credentials are read from slots and never copied.

**Tech Stack:** Rust (edition per repo), `tiny_http 0.12`, `ureq 3.3` (default features: rustls + bundled webpki roots), existing `Secret`/`atomic`/`slots`/`quota` modules.

**Design:** [`../specs/2026-07-27-swapdex-proxy-mode-design.md`](../specs/2026-07-27-swapdex-proxy-mode-design.md)

## Global Constraints

- Repo content (code, comments, docs, commits) in English. No emoji anywhere.
- Unix only: Linux, WSL, macOS. No Windows-native code paths.
- No async runtime. Synchronous, thread-per-connection only.
- TLS via ureq's default rustls + bundled `webpki-roots`. Never `native-tls` (per-platform trust differences).
- **Never log prompt content and never log a token value.** Logs carry time, account name, HTTP status, and quota state only.
- Bind `127.0.0.1` only; refuse any other bind address.
- **Never write a credential outside its own slot.** Tokens live in memory as `Secret` for the request only.
- Only slot-model accounts are proxy-eligible. Legacy copy-model profiles are refused with a pointer to `swapdex run <name>`.
- Tests are hermetic: no real network. A fake upstream is injected via the `SWAPDEX_UPSTREAM` env var (the `SWAPDEX_CURL` precedent in `src/quota.rs`).
- TDD per task: write the failing test, watch it fail, minimal implementation, watch it pass, commit.
- Every task ends green: `cargo test`, `cargo clippy --all-targets` (0 warnings), `cargo fmt --check`.

## File Structure

**New:**
- `src/proxy/mod.rs` — server lifecycle: bind, accept loop, per-connection thread, request handling, log line.
- `src/proxy/upstream.rs` — the ureq agent: forward a request, return status + headers + a streaming reader.
- `src/proxy/ratelimit.rs` — parse `anthropic-ratelimit-unified-*` response headers into a per-account quota state.
- `src/proxy/pick.rs` — pure account selection and rotation decisions (pin vs rotation, eligibility).
- `src/proxy/creds.rs` — read a slot's token (file or macOS Keychain by explicit service) and refresh an idle slot's token.
- `tests/proxy.rs` — hermetic integration tests with a fake upstream.

**Modified:**
- `Cargo.toml` — add `tiny_http`, `ureq`.
- `src/lib.rs` — `pub mod proxy;`
- `src/main.rs` — `Cmd::Proxy` subcommand.
- `src/commands.rs` — `pub fn proxy(...)` entry point; `proxy_status`.
- `src/adapters/claude.rs` — a Keychain read variant taking an explicit service name (today's reader derives the service from the environment).
- `src/tui.rs` — tool-grouped main screen (Task 7).

---

### Task 1: Read a slot's token

The proxy must read any slot's credential, not just the environment's. On Linux/WSL that is the slot's file; on macOS it is the slot's own Keychain item.

**Files:**
- Create: `src/proxy/creds.rs`
- Create: `src/proxy/mod.rs` (module declarations only)
- Modify: `src/lib.rs` (add `pub mod proxy;`)
- Modify: `src/adapters/claude.rs` (add `keychain_read_service`, reuse in `slot_token`)

**Interfaces:**
- Consumes: `crate::secret::Secret`, `crate::adapters::claude::slot_login` (existing tri-state), `keychain_enabled()`.
- Produces: `pub fn slot_token(dir: &Path) -> Option<Secret>` — the access token for that slot, or `None` when the slot has no readable login.

- [ ] **Step 1: Write the failing test**

In `src/proxy/creds.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slot_token_reads_the_access_token_from_the_slot_file() {
        let dir = tempfile::tempdir().unwrap();
        assert!(slot_token(dir.path()).is_none(), "no login yet");
        std::fs::write(
            dir.path().join(".credentials.json"),
            br#"{"claudeAiOauth":{"accessToken":"AT-1","refreshToken":"RT-1","expiresAt":1}}"#,
        )
        .unwrap();
        let t = slot_token(dir.path()).expect("token");
        assert_eq!(t.expose(), b"AT-1");
    }

    #[test]
    fn slot_token_is_none_for_an_unparseable_credential() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(".credentials.json"), b"not json").unwrap();
        assert!(slot_token(dir.path()).is_none());
    }
}
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test --lib proxy::creds`
Expected: FAIL to compile — `slot_token` does not exist.

- [ ] **Step 3: Write the minimal implementation**

`src/proxy/mod.rs`:

```rust
//! Proxy mode: a loopback HTTP server that forwards Claude Code's API traffic
//! upstream, choosing the account per request so a running conversation can
//! change accounts. Credentials are read from slots and never copied.

pub mod creds;
```

`src/lib.rs`: add `pub mod proxy;` next to the other modules.

`src/proxy/creds.rs`:

```rust
//! Reading a slot's own credential. The slot is the single source of truth: the
//! token is held in memory for one request and never written anywhere else.

use crate::secret::Secret;
use std::path::Path;

/// The access token stored in this slot, or `None` when the slot has no
/// readable login. Linux/WSL: the slot's `.credentials.json`. macOS: the slot's
/// own Keychain item (attribute-derived service name).
pub fn slot_token(dir: &Path) -> Option<Secret> {
    if let Ok(bytes) = std::fs::read(dir.join(".credentials.json")) {
        if let Some(t) = access_token(&bytes) {
            return Some(t);
        }
    }
    let bytes = crate::adapters::claude::slot_keychain_read(dir)?;
    access_token(&bytes)
}

/// Pull `claudeAiOauth.accessToken` out of a Claude credential blob.
fn access_token(bytes: &[u8]) -> Option<Secret> {
    let v: serde_json::Value = serde_json::from_slice(bytes).ok()?;
    let t = v["claudeAiOauth"]["accessToken"].as_str()?;
    (!t.is_empty()).then(|| Secret::new(t.as_bytes().to_vec()))
}
```

In `src/adapters/claude.rs`, add next to `slot_login`:

```rust
/// Read a SLOT's Keychain credential blob (macOS). The service name is derived
/// from the slot's config dir the way Claude Code derives it, so this reads that
/// account's own item rather than the environment-derived one. `None` off macOS,
/// under SWAPDEX_ROOT, or when the item does not exist.
pub(crate) fn slot_keychain_read(dir: &std::path::Path) -> Option<Vec<u8>> {
    if !keychain_enabled() {
        return None;
    }
    let service = format!(
        "{KEYCHAIN_PREFIX}-{}",
        &sha256_hex(dir.to_string_lossy().as_bytes())[..8]
    );
    let out = std::process::Command::new(SECURITY)
        .args([
            "find-generic-password",
            "-s",
            &service,
            "-a",
            &keychain_account_name(),
            "-w",
        ])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let mut v = out.stdout;
    while v.last().is_some_and(|b| *b == b'\n' || *b == b'\r') {
        v.pop();
    }
    (!v.is_empty()).then_some(v)
}
```

Make `slot_keychain_read` visible to `proxy::creds` (it is `pub(crate)`; if `Secret`/module paths require it, re-export rather than widening visibility).

- [ ] **Step 4: Run the test to verify it passes**

Run: `cargo test --lib proxy::creds`
Expected: PASS (2 tests). Keychain path is not exercised in unit tests (`keychain_enabled()` is false under `cfg(test)`), by design.

- [ ] **Step 5: Commit**

```bash
git add src/lib.rs src/proxy/mod.rs src/proxy/creds.rs src/adapters/claude.rs
git commit -m "feat(proxy): read a slot's own access token (file or slot Keychain item)"
```

---

### Task 2: Pass-through proxy with token injection

The proxy forwards a request upstream with the chosen slot's token and streams the response back. This is also the feasibility check: run the real `claude` against it.

**Files:**
- Create: `src/proxy/upstream.rs`
- Modify: `src/proxy/mod.rs` (server + request handling)
- Modify: `Cargo.toml`, `src/main.rs`, `src/commands.rs`
- Create: `tests/proxy.rs`

**Interfaces:**
- Consumes: `creds::slot_token`, `crate::slots::Slots`, `crate::paths::Paths`.
- Produces:
  - `upstream::forward(agent, method, url, headers, body) -> Result<Upstream>` where `pub struct Upstream { pub status: u16, pub headers: Vec<(String, String)>, pub reader: Box<dyn Read + Send> }`
  - `proxy::serve(paths: &Paths, opts: &Opts) -> Result<()>`
  - `pub struct Opts { pub port: u16, pub account: Option<String>, pub auto: bool }`

- [ ] **Step 1: Write the failing test**

`tests/proxy.rs`:

```rust
use std::io::Read;
use std::process::Command;

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_swapdex")
}

/// A fake upstream: records the Authorization it received, answers with a body.
fn fake_upstream(auth_sink: std::sync::Arc<std::sync::Mutex<Vec<String>>>) -> (String, u16) {
    let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
    let port = server.server_addr().to_ip().unwrap().port();
    std::thread::spawn(move || {
        for mut rq in server.incoming_requests() {
            let auth = rq
                .headers()
                .iter()
                .find(|h| h.field.equiv("authorization"))
                .map(|h| h.value.as_str().to_string())
                .unwrap_or_default();
            let mut body = Vec::new();
            rq.as_reader().read_to_end(&mut body).ok();
            auth_sink.lock().unwrap().push(auth);
            let _ = rq.respond(tiny_http::Response::from_string("{\"ok\":true}"));
        }
    });
    (format!("http://127.0.0.1:{port}"), port)
}

#[test]
fn proxy_injects_the_slots_token_and_streams_the_response_back() {
    let root = tempfile::tempdir().unwrap();
    // A slot with a known token.
    let slot = root
        .path()
        .join(".local/share/swapdex/slots/aaaa1111");
    std::fs::create_dir_all(&slot).unwrap();
    std::fs::write(
        slot.join(".credentials.json"),
        br#"{"claudeAiOauth":{"accessToken":"AT-SLOT","refreshToken":"R","expiresAt":9999999999999}}"#,
    )
    .unwrap();
    std::fs::write(
        root.path().join(".local/share/swapdex/slots.json"),
        format!(
            r#"[{{"name":"work","id":"aaaa1111","config_dir":"{}","adopted":false}}]"#,
            slot.display()
        ),
    )
    .unwrap();
    std::fs::write(
        root.path().join(".local/share/swapdex/active-claude"),
        slot.to_string_lossy().as_bytes(),
    )
    .unwrap();

    let sink = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let (upstream, _p) = fake_upstream(sink.clone());

    let mut child = Command::new(bin())
        .args(["proxy", "--port", "0"])
        .env("SWAPDEX_ROOT", root.path())
        .env("SWAPDEX_UPSTREAM", &upstream)
        .stdout(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    // The proxy prints "listening on http://127.0.0.1:<port>" first.
    let port = {
        let out = child.stdout.as_mut().unwrap();
        let mut buf = Vec::new();
        let mut b = [0u8; 1];
        while out.read(&mut b).unwrap_or(0) == 1 {
            if b[0] == b'\n' {
                break;
            }
            buf.push(b[0]);
        }
        let line = String::from_utf8_lossy(&buf).to_string();
        line.rsplit(':').next().unwrap().trim().parse::<u16>().unwrap()
    };

    let resp = ureq::post(&format!("http://127.0.0.1:{port}/v1/messages"))
        .header("authorization", "Bearer CLIENT-TOKEN")
        .header("content-type", "application/json")
        .send(&b"{\"model\":\"x\"}"[..]);
    let mut resp = resp.expect("proxy answered");
    let mut body = String::new();
    resp.body_mut().as_reader().read_to_string(&mut body).unwrap();

    child.kill().ok();
    assert!(body.contains("\"ok\":true"), "response streamed back: {body}");
    let seen = sink.lock().unwrap().clone();
    assert_eq!(
        seen,
        vec!["Bearer AT-SLOT".to_string()],
        "the slot's token replaced the client's"
    );
}
```

Add to `Cargo.toml` dev-dependencies so the test can use them:

```toml
[dev-dependencies]
tiny_http = "0.12"
ureq = "3.3"
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test --test proxy proxy_injects_the_slots_token_and_streams_the_response_back`
Expected: FAIL — `swapdex proxy` is not a subcommand (`error: unrecognized subcommand`).

- [ ] **Step 3: Write the minimal implementation**

`Cargo.toml` dependencies:

```toml
tiny_http = "0.12"
ureq = "3.3"
```

`src/proxy/upstream.rs`:

```rust
//! The upstream leg: forward one request to the API and hand back a streaming
//! reader. Synchronous by design - one thread owns one request end to end.

use anyhow::{Context, Result};
use std::io::Read;

/// Where the API lives. `SWAPDEX_UPSTREAM` redirects it for hermetic tests -
/// the same fixture pattern as `SWAPDEX_CURL` in `quota.rs`, so no test ever
/// reaches the real API.
pub fn base_url() -> String {
    std::env::var("SWAPDEX_UPSTREAM")
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "https://api.anthropic.com".to_string())
}

pub struct Upstream {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub reader: Box<dyn Read + Send>,
}

/// An agent that returns 4xx/5xx as responses instead of errors: a 429 carries
/// the rate-limit headers we need, so it must not be swallowed as an error.
pub fn agent() -> ureq::Agent {
    ureq::Agent::config_builder()
        .http_status_as_error(false)
        .build()
        .into()
}

/// Forward one request. `headers` is passed through verbatim except the
/// Authorization the caller already replaced.
pub fn forward(
    agent: &ureq::Agent,
    method: &str,
    url: &str,
    headers: &[(String, String)],
    body: &[u8],
) -> Result<Upstream> {
    let mut rb = match method {
        "GET" => agent.get(url).into_builder_with_body(),
        _ => agent.post(url),
    };
    for (k, v) in headers {
        rb = rb.header(k.as_str(), v.as_str());
    }
    let resp = rb.send(body).context("upstream request failed")?;
    let status = resp.status().as_u16();
    let headers = resp
        .headers()
        .iter()
        .filter_map(|(n, v)| v.to_str().ok().map(|s| (n.as_str().to_string(), s.to_string())))
        .collect();
    Ok(Upstream {
        status,
        headers,
        reader: Box::new(resp.into_body().into_reader()),
    })
}
```

> Note for the implementer: `ureq 3` builders are typed by whether a body is
> allowed (`RequestBuilder<WithBody>` vs `WithoutBody`). If the `GET` arm above
> does not type-check, handle GET and POST in two separate match arms that each
> send and convert to `Upstream`, rather than unifying the builder type.

`src/proxy/mod.rs` (replace the module-only stub):

```rust
//! Proxy mode: a loopback HTTP server that forwards Claude Code's API traffic
//! upstream, choosing the account per request so a running conversation can
//! change accounts. Credentials are read from slots and never copied, and
//! neither prompt content nor any token value is ever logged.

pub mod creds;
pub mod upstream;

use crate::paths::Paths;
use anyhow::{anyhow, Result};
use std::io::Read;
use std::sync::Arc;

pub struct Opts {
    pub port: u16,
    pub account: Option<String>,
    /// Continue on another account when the current one is spent (Task 6).
    pub auto: bool,
}

/// Headers that must not be forwarded upstream: hop-by-hop, or ones ureq sets
/// itself from the body/connection.
fn skip_header(name: &str) -> bool {
    matches!(
        name.to_ascii_lowercase().as_str(),
        "host" | "connection" | "content-length" | "transfer-encoding" | "accept-encoding"
    )
}

/// The slot the next request should use: the `active-claude` pointer resolved to
/// a slot record, or the only slot when no pointer is set.
fn pick_slot(paths: &Paths, opts: &Opts) -> Result<crate::slots::SlotRecord> {
    let slots = crate::slots::Slots::open(paths)?;
    let list = slots.list();
    if let Some(name) = &opts.account {
        return slots
            .get(name)
            .ok_or_else(|| anyhow!("no account slot named '{name}' - `swapdex slots` lists them"));
    }
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
    let server = tiny_http::Server::http(("127.0.0.1", opts.port))
        .map_err(|e| anyhow!("cannot bind 127.0.0.1:{}: {e}", opts.port))?;
    let port = server
        .server_addr()
        .to_ip()
        .ok_or_else(|| anyhow!("no TCP port"))?
        .port();
    println!("swapdex proxy listening on http://127.0.0.1:{port}");
    println!("  point Claude at it:  export ANTHROPIC_BASE_URL=http://127.0.0.1:{port}");
    use std::io::Write;
    std::io::stdout().flush().ok();

    let server = Arc::new(server);
    let agent = Arc::new(upstream::agent());
    let base = upstream::base_url();
    loop {
        let rq = match server.recv() {
            Ok(r) => r,
            Err(_) => continue,
        };
        let (paths, agent, base) = (paths.clone(), agent.clone(), base.clone());
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
        .map(|h| (h.field.as_str().as_str().to_string(), h.value.as_str().to_string()))
        .collect();
    headers.push((
        "authorization".into(),
        format!("Bearer {}", String::from_utf8_lossy(token.expose())),
    ));

    let url = format!("{base}{}", rq.url());
    let mut body = Vec::new();
    rq.as_reader().read_to_end(&mut body)?;

    let up = upstream::forward(agent, rq.method().as_str(), &url, &headers, &body)?;
    println!("{} {} -> {}", slot.name, rq.url(), up.status);

    let out_headers: Vec<tiny_http::Header> = up
        .headers
        .iter()
        .filter(|(n, _)| !skip_header(n))
        .filter_map(|(n, v)| tiny_http::Header::from_bytes(n.as_bytes(), v.as_bytes()).ok())
        .collect();
    // Length is unknown (streamed/SSE), so respond chunked.
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
```

`src/main.rs` — add to the `Cmd` enum and the dispatch:

```rust
    /// Run a local proxy so a running Claude session can change accounts
    Proxy {
        /// Port to listen on (0 picks a free one)
        #[arg(long, default_value_t = 8787)]
        port: u16,
        /// Use this account instead of the current default
        #[arg(long)]
        account: Option<String>,
    },
```

```rust
        Cmd::Proxy { port, account } => commands::proxy(&paths, *port, account.clone()),
```

`src/commands.rs`:

```rust
/// Run proxy mode (foreground).
pub fn proxy(paths: &Paths, port: u16, account: Option<String>, auto: bool) -> Result<i32> {
    let opts = crate::proxy::Opts {
        port,
        account,
        auto,
    };
    crate::proxy::serve(paths, &opts)?;
    Ok(0)
}
```

`Paths` must be `Clone` for the per-thread move; if it is not, add `#[derive(Clone)]` to it (it holds only paths).

`handle`'s signature grows over the next tasks as shared state appears: Task 3
adds `state: &Arc<Mutex<HashMap<String, ratelimit::Quota>>>`, Task 4 adds
`chooser: &Mutex<pick::Chooser>`, Task 6 adds
`rotated: &Mutex<Option<String>>`. Create each in `serve` and clone the `Arc`
into the per-request thread alongside `agent`.

- [ ] **Step 4: Run the test to verify it passes**

Run: `cargo test --test proxy`
Expected: PASS — the response body came back and the fake upstream saw `Bearer AT-SLOT`.

- [ ] **Step 5: Feasibility check against the real client (manual, once)**

This is the gate the whole design rests on: does a subscription (OAuth) `claude` honor `ANTHROPIC_BASE_URL` pointed at a plain-HTTP loopback proxy?

```bash
swapdex proxy --port 8787          # terminal 1
ANTHROPIC_BASE_URL=http://127.0.0.1:8787 claude   # terminal 2, ask it anything
```

Expected: the proxy prints one line per turn (`<account> /v1/messages -> 200`) and Claude answers normally.
If Claude refuses or bypasses the base URL, STOP and report — the design's §3 assumption is wrong and MITM-vs-alternative has to be reconsidered before Task 3.

- [ ] **Step 6: Record the observed protocol**

While the real client runs, note from the proxy's own log (add temporary eprintln of header NAMES only — never values, never bodies):
- the exact `anthropic-ratelimit-unified-*` header names present on a 200,
- whether the request body carries an account identifier field (e.g. `metadata.user_id`),
- the URL/shape of the client's own token-refresh request.

Write the findings into the design spec's §4.2 so Tasks 3-5 are built on facts. Remove the temporary logging before committing.

- [ ] **Step 7: Commit**

```bash
git add Cargo.toml Cargo.lock src/proxy/ src/main.rs src/commands.rs src/paths.rs tests/proxy.rs docs/superpowers/specs/2026-07-27-swapdex-proxy-mode-design.md
git commit -m "feat(proxy): pass-through proxy that injects the chosen slot's token"
```

---

### Task 3: Capture quota state from response headers

Every upstream response carries the account's rate-limit state. Reading it is free telemetry: no probing, no keep-warm.

**Files:**
- Create: `src/proxy/ratelimit.rs`
- Modify: `src/proxy/mod.rs` (record state per account after each response)

**Interfaces:**
- Produces:
  - `pub struct Quota { pub rejected: bool, pub statuses: Vec<(String, String)>, pub reset_secs: Option<i64> }`
  - `pub fn from_headers(headers: &[(String, String)]) -> Option<Quota>`
  - `pub type State = std::sync::Mutex<std::collections::HashMap<String, Quota>>` (account name -> last seen)

- [ ] **Step 1: Write the failing test**

In `src/proxy/ratelimit.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    fn h(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
        pairs.iter().map(|(a, b)| (a.to_string(), b.to_string())).collect()
    }

    #[test]
    fn allowed_status_is_not_rejected() {
        let q = from_headers(&h(&[
            ("anthropic-ratelimit-unified-status", "allowed"),
            ("anthropic-ratelimit-unified-5h-status", "allowed"),
        ]))
        .expect("quota seen");
        assert!(!q.rejected);
    }

    #[test]
    fn any_rejected_status_marks_the_account_exhausted() {
        let q = from_headers(&h(&[
            ("anthropic-ratelimit-unified-status", "allowed"),
            ("anthropic-ratelimit-unified-7d-status", "rejected"),
        ]))
        .expect("quota seen");
        assert!(q.rejected, "a rejected window exhausts the account");
    }

    #[test]
    fn reset_is_parsed_and_absent_headers_yield_none() {
        let q = from_headers(&h(&[
            ("anthropic-ratelimit-unified-status", "allowed_warning"),
            ("anthropic-ratelimit-unified-reset", "1800000000"),
        ]))
        .expect("quota seen");
        assert_eq!(q.reset_secs, Some(1_800_000_000));
        assert!(from_headers(&h(&[("content-type", "application/json")])).is_none());
    }
}
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test --lib proxy::ratelimit`
Expected: FAIL to compile — `from_headers` does not exist.

- [ ] **Step 3: Write the minimal implementation**

```rust
//! Quota state read off the response headers Anthropic already sends. Header
//! NAMES are matched by prefix rather than an exact list, so an added window
//! (5h/7d/other) is picked up without a code change; `rejected` in any window
//! means the account is spent. Nothing here probes the API.

const PREFIX: &str = "anthropic-ratelimit-unified-";

#[derive(Debug, Default, Clone)]
pub struct Quota {
    /// Any window reported `rejected` - this account is spent.
    pub rejected: bool,
    /// Every `*-status` header seen, for display and diagnosis.
    pub statuses: Vec<(String, String)>,
    /// Soonest reset epoch seconds, when reported.
    pub reset_secs: Option<i64>,
}

/// `None` when the response carried no unified rate-limit headers at all (so a
/// non-API response never overwrites a known-good state with an empty one).
pub fn from_headers(headers: &[(String, String)]) -> Option<Quota> {
    let mut q = Quota::default();
    let mut seen = false;
    for (name, value) in headers {
        let lower = name.to_ascii_lowercase();
        let Some(rest) = lower.strip_prefix(PREFIX) else {
            continue;
        };
        seen = true;
        if rest == "status" || rest.ends_with("-status") {
            if value.eq_ignore_ascii_case("rejected") {
                q.rejected = true;
            }
            q.statuses.push((rest.to_string(), value.clone()));
        } else if rest == "reset" || rest.ends_with("-reset") {
            if let Ok(n) = value.trim().parse::<i64>() {
                q.reset_secs = Some(q.reset_secs.map_or(n, |cur| cur.min(n)));
            }
        }
    }
    seen.then_some(q)
}
```

In `src/proxy/mod.rs`: declare `pub mod ratelimit;`, hold a shared
`Arc<Mutex<HashMap<String, ratelimit::Quota>>>` created in `serve` and passed into
`handle`, and after a response record it:

```rust
    if let Some(q) = ratelimit::from_headers(&up.headers) {
        let spent = if q.rejected { " SPENT" } else { "" };
        println!("{} {} -> {}{spent}", slot.name, rq.url(), up.status);
        state.lock().unwrap().insert(slot.name.clone(), q);
    } else {
        println!("{} {} -> {}", slot.name, rq.url(), up.status);
    }
```

- [ ] **Step 4: Run the test to verify it passes**

Run: `cargo test --lib proxy::ratelimit && cargo test --test proxy`
Expected: PASS (3 new unit tests; the Task 2 integration test still green).

- [ ] **Step 5: Commit**

```bash
git add src/proxy/
git commit -m "feat(proxy): read per-account quota state from rate-limit response headers"
```

---

### Task 4: Move the running conversation to a chosen account

The user's ask: switch accounts *now*, from the CLI or the dashboard, without restarting Claude. The pointer `swapdex use` already writes is re-read per request, so no new machinery is needed — plus a rule for how a user's explicit choice interacts with an automatic rotation.

**Files:**
- Create: `src/proxy/pick.rs`
- Modify: `src/proxy/mod.rs` (use `pick`), `tests/proxy.rs`

**Interfaces:**
- Produces:
  - `pub struct Chooser { pinned: Option<PathBuf>, current: Option<String> }`
  - `pub fn choose(&mut self, pointer: Option<&Path>, rotated: Option<&str>, slots: &[SlotRecord]) -> Option<SlotRecord>`
  - Rule: a pointer value that CHANGED since the last request wins (the user just chose); otherwise a rotation choice stands; otherwise the pointer.

- [ ] **Step 1: Write the failing test**

`src/proxy/pick.rs`:

```rust
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
            c.choose(Some(&PathBuf::from("/s/rnd")), None, &slots).unwrap().name,
            "rnd"
        );
        // Quota rotated us to bsgong; the unchanged pointer must not undo it.
        assert_eq!(
            c.choose(Some(&PathBuf::from("/s/rnd")), Some("bsgong"), &slots)
                .unwrap()
                .name,
            "bsgong"
        );
        // The user then picks rnd explicitly (pointer CHANGED) - that wins.
        assert_eq!(
            c.choose(Some(&PathBuf::from("/s/bsgong")), Some("bsgong"), &slots)
                .unwrap()
                .name,
            "bsgong"
        );
        assert_eq!(
            c.choose(Some(&PathBuf::from("/s/rnd")), Some("bsgong"), &slots)
                .unwrap()
                .name,
            "rnd",
            "an explicit new choice overrides the rotation"
        );
    }

    #[test]
    fn unknown_pointer_falls_back_to_the_first_slot() {
        let slots = vec![slot("rnd", "/s/rnd")];
        let mut c = Chooser::default();
        assert_eq!(
            c.choose(Some(&PathBuf::from("/nope")), None, &slots).unwrap().name,
            "rnd"
        );
        assert!(c.choose(None, None, &[]).is_none(), "no slots -> no choice");
    }
}
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test --lib proxy::pick`
Expected: FAIL to compile — `Chooser` does not exist.

- [ ] **Step 3: Write the minimal implementation**

```rust
//! Which account serves the next request. Two inputs can disagree: the user's
//! pointer (`swapdex use` / the TUI) and the proxy's own rotation after a spent
//! account. The rule: a pointer that CHANGED since the last request is a fresh
//! human decision and wins; otherwise a rotation stands.

use crate::slots::SlotRecord;
use std::path::{Path, PathBuf};

#[derive(Default)]
pub struct Chooser {
    last_pointer: Option<PathBuf>,
    seen_once: bool,
}

impl Chooser {
    pub fn choose(
        &mut self,
        pointer: Option<&Path>,
        rotated: Option<&str>,
        slots: &[SlotRecord],
    ) -> Option<SlotRecord> {
        let changed = self.seen_once && pointer.map(Path::to_path_buf) != self.last_pointer;
        self.last_pointer = pointer.map(Path::to_path_buf);
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
```

In `src/proxy/mod.rs`: replace `pick_slot`'s pointer logic with a shared
`Mutex<pick::Chooser>` created in `serve`, reading `slots.default_dir()` per
request, passing the rotation choice (`None` until Task 6). Keep `--account` as
an absolute override that skips the Chooser entirely.

- [ ] **Step 4: Run the test to verify it passes**

Run: `cargo test --lib proxy::pick && cargo test --test proxy`
Expected: PASS.

- [ ] **Step 5: Add the integration test for a live switch**

Add to `tests/proxy.rs`: with the proxy running, write a second slot into
`slots.json`, point `active-claude` at it, send another request, and assert the
fake upstream saw the SECOND slot's token — the conversation moved accounts with
no restart.

Run: `cargo test --test proxy`
Expected: PASS.

- [ ] **Step 6: Commit**

```bash
git add src/proxy/ tests/proxy.rs
git commit -m "feat(proxy): follow the account pointer per request so a live session can switch"
```

---

### Task 5: Refresh an idle slot's token (safely)

Rotating onto an account that has not run for hours needs that slot's access token refreshed. This is the narrow exception the design spells out — with rules that keep it from becoming the logout bug it replaced.

**Files:**
- Modify: `src/proxy/creds.rs` (refresh), `src/proxy/mod.rs` (call it before use)

**Interfaces:**
- Produces:
  - `pub fn needs_refresh(dir: &Path, now_ms: i64) -> bool` — expiry within a 5-minute margin.
  - `pub fn refresh_slot(paths: &Paths, slot: &SlotRecord) -> Result<Secret>` — refresh and persist in place.
  - `pub fn refresh_allowed(slot: &SlotRecord, running: &[String]) -> bool` — refuse when a session is live on that slot.

- [ ] **Step 1: Write the failing test**

```rust
    #[test]
    fn needs_refresh_uses_a_five_minute_margin() {
        let dir = tempfile::tempdir().unwrap();
        let write = |exp: i64| {
            std::fs::write(
                dir.path().join(".credentials.json"),
                format!(r#"{{"claudeAiOauth":{{"accessToken":"A","expiresAt":{exp}}}}}"#),
            )
            .unwrap()
        };
        let now = 1_000_000_000_000i64;
        write(now + 10 * 60 * 1000);
        assert!(!needs_refresh(dir.path(), now), "10 min left is fine");
        write(now + 60 * 1000);
        assert!(needs_refresh(dir.path(), now), "1 min left needs a refresh");
        write(now - 1000);
        assert!(needs_refresh(dir.path(), now), "already expired");
    }

    #[test]
    fn refresh_is_refused_while_a_session_runs_on_that_slot() {
        let rec = crate::slots::SlotRecord {
            name: "rnd".into(),
            id: "i".into(),
            config_dir: std::path::PathBuf::from("/s/rnd"),
            adopted: false,
        };
        assert!(refresh_allowed(&rec, &[]), "idle slot may be refreshed");
        assert!(
            !refresh_allowed(&rec, &["/s/rnd".to_string()]),
            "a live session on that slot refreshes its own token - never race it"
        );
    }
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test --lib proxy::creds`
Expected: FAIL to compile — `needs_refresh` / `refresh_allowed` do not exist.

- [ ] **Step 3: Write the minimal implementation**

```rust
/// Refresh margin: a token this close to expiry is treated as expired.
const REFRESH_MARGIN_MS: i64 = 5 * 60 * 1000;

/// Does this slot's access token need refreshing before use?
pub fn needs_refresh(dir: &Path, now_ms: i64) -> bool {
    let Some(exp) = std::fs::read(dir.join(".credentials.json"))
        .ok()
        .and_then(|b| serde_json::from_slice::<serde_json::Value>(&b).ok())
        .and_then(|v| v["claudeAiOauth"]["expiresAt"].as_i64())
    else {
        // No readable expiry (macOS Keychain, or absent): let the request try
        // and let a 401 fail the account out - never guess a refresh.
        return false;
    };
    exp - now_ms <= REFRESH_MARGIN_MS
}

/// A slot whose own `claude` is running refreshes its token itself; refreshing
/// it from here is exactly the rotation collision the slot model removed.
/// `running` holds the config dirs of live Claude sessions.
pub fn refresh_allowed(slot: &crate::slots::SlotRecord, running: &[String]) -> bool {
    let dir = slot.config_dir.to_string_lossy();
    !running.iter().any(|r| r == &dir)
}
```

Then `refresh_slot`. **This step is gated on Task 2 Step 6**: the token endpoint
and the exact grant fields are not documented, and inventing them risks burning a
refresh token. Do not guess — implement it only after the client's own refresh
request has been observed, filling the recorded URL and field names into this
skeleton:

```rust
/// Refresh this slot's login in place and return the new access token. Persists
/// to the slot's OWN store (file or its Keychain item) - never anywhere else, so
/// there is still no second copy to go stale. Callers must have checked
/// `refresh_allowed` first.
pub fn refresh_slot(paths: &Paths, slot: &crate::slots::SlotRecord) -> Result<Secret> {
    let blob = read_slot_blob(&slot.config_dir)
        .ok_or_else(|| anyhow!("account '{}' has no login to refresh", slot.name))?;
    let v: serde_json::Value = serde_json::from_slice(&blob)?;
    let refresh = v["claudeAiOauth"]["refreshToken"]
        .as_str()
        .filter(|s| !s.is_empty())
        .ok_or_else(|| anyhow!("account '{}' has no refresh token", slot.name))?;

    // OBSERVED IN TASK 2 STEP 6 - fill in from the recorded request, do not guess:
    //   const TOKEN_URL: &str = "<observed refresh URL>";
    //   body: {"grant_type":"refresh_token","refresh_token":<refresh>, <observed extra fields>}
    let agent = crate::proxy::upstream::agent();
    let body = serde_json::to_vec(&serde_json::json!({
        "grant_type": "refresh_token",
        "refresh_token": refresh,
    }))?;
    let mut resp = agent
        .post(TOKEN_URL)
        .header("content-type", "application/json")
        .send(&body[..])
        .context("token refresh request failed")?;
    if resp.status().as_u16() != 200 {
        bail!(
            "token refresh for '{}' was rejected ({})",
            slot.name,
            resp.status().as_u16()
        );
    }
    let mut out = Vec::new();
    resp.body_mut().as_reader().read_to_end(&mut out)?;
    let fresh: serde_json::Value = serde_json::from_slice(&out)?;

    // Merge into the slot's existing blob so unrelated fields survive, then
    // persist in place: atomically for the file, or the slot's own Keychain item.
    let merged = merge_oauth(&v, &fresh)?;
    persist_slot_blob(paths, slot, &merged)?;
    access_token(&serde_json::to_vec(&merged)?)
        .ok_or_else(|| anyhow!("refreshed blob has no access token"))
}
```

On any failure return the error; the caller marks the account unusable for this
run and picks another. **Never retry a refresh** — a retry loop is how refresh
tokens get burned.

In `handle`: before using a slot, if `needs_refresh` and `refresh_allowed`
(using `crate::proc` for live sessions), refresh once; on error, log
`account '<name>': login needs a re-run (swapdex run <name>)` and rotate on.

- [ ] **Step 4: Run the test to verify it passes**

Run: `cargo test --lib proxy::creds`
Expected: PASS (both new tests).

- [ ] **Step 5: Commit**

```bash
git add src/proxy/
git commit -m "feat(proxy): refresh an idle slot's token in place, never one with a live session"
```

---

### Task 6: Rotate on a spent account (the auto-continue setting)

The user's ask: optionally continue automatically when the limit hits. Rotation happens at a turn boundary — never mid-stream — so no answer is ever severed.

**Files:**
- Modify: `src/proxy/pick.rs` (rotation target), `src/proxy/mod.rs` (trigger), `src/main.rs` + `src/commands.rs` (`--auto`), `tests/proxy.rs`

**Interfaces:**
- Produces: `pub fn rotate_target(current: &str, slots: &[SlotRecord], state: &HashMap<String, Quota>) -> Option<String>` — the next account to try: prefer one not known-spent; skip the current one; `None` when nothing is eligible.

- [ ] **Step 1: Write the failing test**

```rust
    #[test]
    fn rotation_skips_the_current_and_known_spent_accounts() {
        use crate::proxy::ratelimit::Quota;
        let slots = vec![slot("rnd", "/s/rnd"), slot("bsgong", "/s/b"), slot("claude", "/s/c")];
        let mut state = std::collections::HashMap::new();
        state.insert("rnd".to_string(), Quota { rejected: true, ..Default::default() });
        state.insert("bsgong".to_string(), Quota { rejected: true, ..Default::default() });
        assert_eq!(rotate_target("rnd", &slots, &state).as_deref(), Some("claude"));
        state.insert("claude".to_string(), Quota { rejected: true, ..Default::default() });
        assert_eq!(
            rotate_target("rnd", &slots, &state),
            None,
            "every account spent -> nothing to rotate to"
        );
    }
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test --lib proxy::pick`
Expected: FAIL to compile — `rotate_target` does not exist.

- [ ] **Step 3: Write the minimal implementation**

```rust
/// The next account to try after `current` proved spent: the first slot that is
/// not `current` and not known-spent. (A later refinement is "soonest reset
/// first" under sustained load; first-eligible is enough while one account at a
/// time is spent.)
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
```

In `src/proxy/mod.rs` `handle`, after recording quota:

```rust
    // Turn-boundary rotation: this response is already complete, so switching
    // now cannot sever an answer. The NEXT request uses the new account.
    if opts.auto {
        if let Some(q) = ratelimit::from_headers(&up.headers) {
            if q.rejected {
                let slots = crate::slots::Slots::open(paths)?.list();
                let st = state.lock().unwrap();
                match pick::rotate_target(&slot.name, &slots, &st) {
                    Some(next) => {
                        println!("{} is spent - continuing on {next}", slot.name);
                        *rotated.lock().unwrap() = Some(next);
                    }
                    None => println!(
                        "{} is spent and no other account has quota left",
                        slot.name
                    ),
                }
            }
        }
    }
```

Add the flag (this is the "auto-continue" setting the user asked for):

```rust
        /// Continue on another account automatically when this one is spent
        #[arg(long)]
        auto: bool,
```

Pre-emptive rotation at a utilization threshold is deliberately NOT in v1: it
needs a utilization percentage, and whether the response headers carry one is
unknown until Task 2 Step 6 records them. v1 rotates on a definitive `rejected`,
which needs no guess. Add `--threshold` only once there is a number to compare
against, so the flag is never dead.

- [ ] **Step 4: Run the test to verify it passes**

Run: `cargo test --lib proxy::pick`
Expected: PASS.

- [ ] **Step 5: Add the integration test for automatic rotation**

Extend the fake upstream to answer the FIRST request with
`anthropic-ratelimit-unified-status: rejected`, then normally. Run the proxy with
`--auto`, send two requests, and assert the second carried the SECOND slot's
token. Also assert the first response still reached the client intact (rotation
must not swallow a completed turn).

Run: `cargo test --test proxy`
Expected: PASS.

- [ ] **Step 6: Verify the actual goal with the real client (manual)**

This is what the whole plan is for. With a real conversation running:

```bash
swapdex proxy --auto                                    # terminal 1
ANTHROPIC_BASE_URL=http://127.0.0.1:8787 claude         # terminal 2
```

1. Ask something; confirm the proxy logs the current account.
2. In terminal 3, `swapdex use <other-account>` (or press Enter on it in
   `swapdex ui`).
3. Ask something else in the SAME conversation. The proxy must log the new
   account, and Claude must answer with the conversation intact — no new chat,
   no resume.

Report which accounts were used for which turn. If the switch does not take
effect, the pointer/Chooser wiring in Task 4 is wrong — fix that before moving on.

- [ ] **Step 7: Commit**

```bash
git add src/proxy/ src/main.rs src/commands.rs tests/proxy.rs
git commit -m "feat(proxy): rotate to another account at a turn boundary when one is spent"
```

---

### Task 7: Tool-grouped dashboard that switches the live session

The user's ask: the CLI dashboard groups accounts by tool (`claude ---` / `codex ---`, only for tools that exist) with usage visible, and picking one moves the running conversation. The switch already works through the pointer (Task 4); this task is the grouping.

**Files:**
- Modify: `src/tui.rs`

**Interfaces:**
- Produces:
  - `pub enum MainItem { Group(&'static str), Account(Row) }`
  - `pub fn group_items(rows: Vec<Row>) -> Vec<MainItem>` — accounts sorted by canonical tool order, each run preceded by its group header; a tool with no accounts emits no header.
  - `fn next_selectable(items: &[MainItem], from: usize, down: bool) -> usize` — arrow keys skip headers.

- [ ] **Step 1: Write the failing test**

```rust
    #[test]
    fn group_items_emits_a_header_per_present_tool_only() {
        let row = |name: &str, tools: &str| Row {
            name: name.into(),
            ident: "e@x".into(),
            tools: tools.into(),
            active: false,
            warn: None,
        };
        let items = group_items(vec![
            row("codex", "codex*"),
            row("rnd", "claude-code*"),
            row("bsgong", "claude-code"),
        ]);
        let shape: Vec<String> = items
            .iter()
            .map(|i| match i {
                MainItem::Group(g) => format!("[{g}]"),
                MainItem::Account(r) => r.name.clone(),
            })
            .collect();
        assert_eq!(
            shape,
            vec!["[claude-code]", "rnd", "bsgong", "[codex]", "codex"],
            "claude accounts first under their header, then codex; no empty groups"
        );
        // Arrow keys never land on a header.
        assert_eq!(next_selectable(&items, 0, true), 1);
        assert_eq!(next_selectable(&items, 2, true), 4, "skips the codex header");
        assert_eq!(next_selectable(&items, 4, false), 2);
    }
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test --lib tui::tests::group_items_emits_a_header_per_present_tool_only`
Expected: FAIL to compile — `MainItem` / `group_items` / `next_selectable` do not exist.

- [ ] **Step 3: Write the minimal implementation**

```rust
/// A main-screen list entry: either a tool group header or an account row.
pub enum MainItem {
    Group(&'static str),
    Account(Row),
}

/// Group accounts under a header per tool, in canonical tool order, emitting a
/// header only for tools that actually have accounts. An account with several
/// tools is listed under its first canonical one, so no row appears twice.
pub fn group_items(rows: Vec<Row>) -> Vec<MainItem> {
    const ORDER: &[&str] = &["claude-code", "codex", "gemini", "antigravity"];
    let mut out = Vec::new();
    let mut placed = vec![false; rows.len()];
    for tool in ORDER {
        let mut header_done = false;
        for (i, r) in rows.iter().enumerate() {
            if placed[i] || !r.tools.contains(tool) {
                continue;
            }
            if !header_done {
                out.push(MainItem::Group(tool));
                header_done = true;
            }
            placed[i] = true;
            out.push(MainItem::Account(Row {
                name: r.name.clone(),
                ident: r.ident.clone(),
                tools: r.tools.clone(),
                active: r.active,
                warn: r.warn,
            }));
        }
    }
    // Anything with an unrecognized tool string still has to be reachable.
    for (i, r) in rows.into_iter().enumerate() {
        if !placed[i] {
            out.push(MainItem::Account(r));
        }
    }
    out
}

/// The next index an arrow key should land on, skipping group headers. Returns
/// `from` when there is nothing selectable in that direction.
fn next_selectable(items: &[MainItem], from: usize, down: bool) -> usize {
    let mut i = from;
    loop {
        let next = if down {
            if i + 1 >= items.len() {
                return from;
            }
            i + 1
        } else {
            if i == 0 {
                return from;
            }
            i - 1
        };
        if matches!(items[next], MainItem::Account(_)) {
            return next;
        }
        i = next;
    }
}
```

Then wire the main screen: build `items` from `ctx.rows()`, render a
`Group` as a dim label line with a rule (`claude-code ------`) and an
`Account` as today's row (identity, tools, the right-aligned quota bar), keep
`clamp_selection` on a selectable index, route arrow keys through
`next_selectable`, and resolve Enter/`o`/`n`/`d` through
`items[i]` (ignoring a header, which selection can no longer land on).

- [ ] **Step 4: Run the test to verify it passes**

Run: `cargo test --lib tui`
Expected: PASS (all tui tests, including the existing `r`-key and selection ones).

- [ ] **Step 5: Verify visually (required - this is a UI change)**

```bash
cargo run -- ui
```
Confirm: one header per present tool, accounts under it, usage bars still
right-aligned, arrow keys never highlight a header, Enter still switches, and an
empty store still shows the onboarding screen. Screenshot or describe what you
saw; a passing test is not visual proof.

- [ ] **Step 6: Commit**

```bash
git add src/tui.rs
git commit -m "feat(tui): group the main screen by tool with per-account usage"
```

---

## After the tasks

- **Verify the macOS Keychain read path on the Mac** (Task 1). Unit tests cannot
  cover it: `keychain_enabled()` is false under `cfg(test)` and in the
  `SWAPDEX_ROOT` sandbox, by design. Confirm on the Mac that a slot whose login
  lives in the Keychain serves a real turn through the proxy — the same way the
  0.25.0 guard's `ps eww` path was verified there.
- Update `CHANGELOG.md` under `[Unreleased]` with the proxy-mode entries (what it does, the opt-in nature, the ToS stance, the Linux/WSL/macOS support, and the "no credential copies" property).
- Update `docs/COMMANDS.md` with `swapdex proxy` (flags, the `ANTHROPIC_BASE_URL` line, the same-environment/loopback constraint).
- Announce and use `superpowers:finishing-a-development-branch` to decide how the work lands.
- `swapdex proxy --status` (design §6) is deferred with v2's state surface: a
  useful status needs the running proxy's live per-account quota state, which v1
  keeps in memory and prints to its own log. v1 ships without it rather than with
  a version that only guesses from the pointer.
- v2 hardening is deliberately NOT in this plan: see the design's §8 (429 classification, warm-up, per-account concurrency caps, connection affinity, bounded retries, 529 retry, disconnect abort, 401 fail-out). Each becomes its own task when v1 is real.
