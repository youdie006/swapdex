//! The upstream leg: forward one request to the API and hand back a streaming
//! reader. Synchronous by design - one thread owns one request end to end, so a
//! client disconnect surfaces as a write error and drops the upstream read with
//! it (no separate cancellation machinery).

use anyhow::{Context, Result};
use std::io::Read;
use std::time::Duration;

/// Where the API lives. `SWAPDEX_UPSTREAM` redirects it for hermetic tests - the
/// same fixture pattern as `SWAPDEX_CURL` in `quota.rs`, so no test ever reaches
/// the real API.
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

/// Read the reason off a failed response WITHOUT costing the client its body.
///
/// The body is a stream, so reading it to explain a refusal would normally take
/// it away from the caller. An error body is small and finite, so it is
/// buffered and the reader replaced with one over those same bytes: the client
/// still receives exactly what the API sent, and the log gains the sentence.
///
/// `None` for a success. Those are the long SSE streams, and buffering one
/// would hold a whole conversation in memory to say nothing.
pub fn explain_failure(up: &mut Upstream) -> Option<String> {
    const CAP: u64 = 64 * 1024;
    if up.status < 400 {
        return None;
    }
    let mut buf = Vec::new();
    let mut limited = (&mut up.reader).take(CAP);
    // A body that cannot be read is still a refusal worth reporting; whatever
    // arrived before the failure is what there is to go on.
    let _ = limited.read_to_end(&mut buf);
    let why = why_refused(&buf);
    up.reader = Box::new(std::io::Cursor::new(buf));
    Some(why)
}

/// The sentence in an error body that says what went wrong.
///
/// A refusal used to reach the log as three digits and nothing else, so a 400
/// the user saw as "API error" had no explanation anywhere on the machine. The
/// API always sends one; it was simply never read.
///
/// Only ERROR bodies pass through here. They are small and they carry the
/// API's own words, not the conversation - and they are cut short regardless,
/// since a log line is not a place to dump a payload.
pub fn why_refused(body: &[u8]) -> String {
    const LIMIT: usize = 300;
    let text = String::from_utf8_lossy(body);
    let pick = serde_json::from_str::<serde_json::Value>(&text)
        .ok()
        .and_then(|v| {
            for path in [&["error", "message"][..], &["detail"][..], &["message"][..]] {
                let mut cur = &v;
                for key in path {
                    match cur.get(key) {
                        Some(next) => cur = next,
                        None => {
                            cur = &serde_json::Value::Null;
                            break;
                        }
                    }
                }
                if let Some(s) = cur.as_str().map(str::trim).filter(|s| !s.is_empty()) {
                    return Some(s.to_string());
                }
            }
            None
        });
    let flat = match pick {
        Some(s) => s,
        None => text.split_whitespace().collect::<Vec<_>>().join(" "),
    };
    if flat.is_empty() {
        return "(empty response body)".to_string();
    }
    if flat.chars().count() > LIMIT {
        return flat.chars().take(LIMIT - 1).collect::<String>() + "…";
    }
    flat
}

/// An agent that returns 4xx/5xx as responses instead of errors: a 429 carries
/// the rate-limit headers rotation depends on, so it must not be swallowed.
/// How long the proxy will wait on an upstream that is not answering.
///
/// `SWAPDEX_UPSTREAM_WAIT_MS` shortens them for tests, honoured ONLY under
/// `SWAPDEX_ROOT` so a production run cannot be given a hair trigger.
fn waits() -> (Duration, Duration) {
    if std::env::var_os("SWAPDEX_ROOT").is_some() {
        if let Some(ms) = std::env::var("SWAPDEX_UPSTREAM_WAIT_MS")
            .ok()
            .and_then(|v| v.parse::<u64>().ok())
        {
            return (Duration::from_millis(ms), Duration::from_millis(ms));
        }
    }
    (Duration::from_secs(10), Duration::from_secs(300))
}

/// The client the proxy relays through.
///
/// It had no timeouts at all. Every retry in this file fires on an ERROR, and a
/// server that accepts and says nothing never produces one - so an upstream
/// that hangs hung the proxy, and the proxy hung the client: measured at two
/// minutes with zero bytes sent and not one line in the log, because the
/// request never got far enough to be logged. A black-holing network - a
/// captive portal, a firewall that DROPs instead of REJECTs, a half-open VPN -
/// is exactly the shape that produces it.
///
/// Bounded: resolving, connecting, and waiting for the response HEADERS.
/// Deliberately NOT bounded: the body. Responses stream, an SSE turn can run for
/// many minutes, and a global or body timeout would cut a working answer in half
/// - which is the failure this proxy exists to avoid.
pub fn agent() -> ureq::Agent {
    let (short, headers) = waits();
    agent_with(short, headers)
}

/// The construction itself, so a test can prove the waits REACH the client
/// rather than only that the numbers are right.
fn agent_with(short: Duration, headers: Duration) -> ureq::Agent {
    ureq::Agent::config_builder()
        .http_status_as_error(false)
        .timeout_resolve(Some(short))
        .timeout_connect(Some(short))
        .timeout_recv_response(Some(headers))
        .build()
        .into()
}

/// Collect a response's headers, dropping any whose value is not valid UTF-8
/// (never the case for the ones we forward or read).
fn collect_headers<T>(resp: &ureq::http::Response<T>) -> Vec<(String, String)> {
    resp.headers()
        .iter()
        .filter_map(|(n, v)| {
            v.to_str()
                .ok()
                .map(|s| (n.as_str().to_string(), s.to_string()))
        })
        .collect()
}

/// Forward one request upstream. `headers` is passed through verbatim - the
/// caller has already replaced Authorization and dropped hop-by-hop headers.
/// Is this failure a blip worth one more attempt?
///
/// A dropped connection, a DNS lookup that did not resolve, a route that
/// vanished for a moment - the network or the server shedding load, where the
/// next attempt usually succeeds. The retry for these was written once and wired
/// into one of seven call sites, so every other path returned the error straight
/// up and it reached the user as a 502. One Mac's proxy log held 102 of them:
/// 72 DNS lookups, 24 broken pipes.
///
/// A refusal from the server is an ANSWER, not a blip, and has to reach the
/// caller so the account logic can act on it. So must a certificate failure,
/// which will not fix itself by being asked again.
pub fn worth_retrying(err: &str) -> bool {
    let e = err.to_ascii_lowercase();
    if e.contains("certificate") || e.contains("http status") {
        return false;
    }
    // A timeout waiting for the RESPONSE has already spent its whole budget.
    // This list was written when the agent had no timeouts at all, so no
    // timeout could reach it; now that the wait is bounded, retrying one
    // multiplies it - four tries here inside four out there is sixteen, and at
    // the response budget that is over an hour of silence. Resolving and
    // connecting are short and worth another go.
    if e.contains("receive response") || e.contains("recv response") {
        return false;
    }
    e.contains("lookup address")
        || e.contains("broken pipe")
        || e.contains("no route to host")
        || e.contains("unexpected end of file")
        || e.contains("connection reset")
        || e.contains("connection refused")
        || e.contains("timeout")
}

pub fn forward(
    agent: &ureq::Agent,
    method: &str,
    url: &str,
    headers: &[(String, String)],
    body: &[u8],
) -> Result<Upstream> {
    // Retried HERE, so every caller is covered. This was written once and wired
    // into one of seven call sites; the other six returned a dropped connection
    // or a failed DNS lookup straight up, and it reached the user as a 502.
    const TRIES: u32 = 4;
    let mut attempt = 0u32;
    loop {
        match forward_once(agent, method, url, headers, body) {
            Ok(u) => return Ok(u),
            Err(e) => {
                let text = format!("{e:#}");
                if attempt + 1 >= TRIES || !worth_retrying(&text) {
                    return Err(e);
                }
                std::thread::sleep(std::time::Duration::from_millis(250u64 << attempt));
                attempt += 1;
            }
        }
    }
}

fn forward_once(
    agent: &ureq::Agent,
    method: &str,
    url: &str,
    headers: &[(String, String)],
    body: &[u8],
) -> Result<Upstream> {
    // ureq types its builder by whether a body is allowed, so bodyless and
    // body-carrying methods cannot share one variable.
    let bodyless = matches!(
        method.to_ascii_uppercase().as_str(),
        "GET" | "HEAD" | "DELETE" | "OPTIONS"
    );
    if bodyless {
        let mut rb = match method.to_ascii_uppercase().as_str() {
            "HEAD" => agent.head(url),
            "DELETE" => agent.delete(url),
            "OPTIONS" => agent.options(url),
            _ => agent.get(url),
        };
        for (k, v) in headers {
            rb = rb.header(k.as_str(), v.as_str());
        }
        let resp = rb.call().context("upstream request failed")?;
        let status = resp.status().as_u16();
        let headers = collect_headers(&resp);
        return Ok(Upstream {
            status,
            headers,
            reader: Box::new(resp.into_body().into_reader()),
        });
    }
    let mut rb = match method.to_ascii_uppercase().as_str() {
        "PUT" => agent.put(url),
        "PATCH" => agent.patch(url),
        _ => agent.post(url),
    };
    for (k, v) in headers {
        rb = rb.header(k.as_str(), v.as_str());
    }
    let resp = rb.send(body).context("upstream request failed")?;
    let status = resp.status().as_u16();
    let headers = collect_headers(&resp);
    Ok(Upstream {
        status,
        headers,
        reader: Box::new(resp.into_body().into_reader()),
    })
}

#[cfg(test)]
mod failure_tests {
    use super::*;

    #[test]
    fn an_error_body_is_reduced_to_the_sentence_that_explains_it() {
        assert_eq!(
            why_refused(br#"{"type":"error","error":{"type":"invalid_request_error","message":"max_tokens: must be <= 8192"}}"#),
            "max_tokens: must be <= 8192"
        );
        // Codex's backend words it differently.
        assert_eq!(
            why_refused(br#"{"detail":"Store must be set to false"}"#),
            "Store must be set to false"
        );
    }

    #[test]
    fn a_body_that_is_not_json_is_still_reported() {
        // Silence is the failure being fixed: an unexplained 400 reaches the
        // user as "API error" and nothing else.
        assert_eq!(why_refused(b"  Bad   Gateway\n\n"), "Bad Gateway");
        assert_eq!(why_refused(b""), "(empty response body)");
    }

    #[test]
    fn a_long_body_is_cut_rather_than_flooding_the_log() {
        let long = format!("{{\"detail\":\"{}\"}}", "x".repeat(900));
        let got = why_refused(long.as_bytes());
        assert!(got.chars().count() <= 300, "{}", got.chars().count());
        assert!(got.ends_with('…'), "{got}");
    }
}

#[cfg(test)]
mod explain_tests {
    use super::*;

    fn resp(status: u16, body: &str) -> Upstream {
        Upstream {
            status,
            headers: Vec::new(),
            reader: Box::new(std::io::Cursor::new(body.as_bytes().to_vec())),
        }
    }

    /// Explaining a refusal must not cost the client its body - it is the only
    /// thing the client has to render.
    #[test]
    fn a_failure_is_explained_and_its_body_still_reaches_the_client() {
        let mut up = resp(
            400,
            r#"{"error":{"message":"max_tokens: must be <= 8192"}}"#,
        );
        assert_eq!(
            explain_failure(&mut up).as_deref(),
            Some("max_tokens: must be <= 8192")
        );
        let mut back = String::new();
        up.reader.read_to_string(&mut back).unwrap();
        assert_eq!(
            back,
            r#"{"error":{"message":"max_tokens: must be <= 8192"}}"#
        );
    }

    /// A success is left alone. Buffering one would hold an entire streamed
    /// conversation in memory to report nothing.
    #[test]
    fn a_success_is_not_read_at_all() {
        let mut up = resp(200, "event: message_start\n\n");
        assert_eq!(explain_failure(&mut up), None);
        let mut back = String::new();
        up.reader.read_to_string(&mut back).unwrap();
        assert_eq!(back, "event: message_start\n\n");
    }
}

#[cfg(test)]
mod transient_retry_tests {
    use super::*;

    /// A transport failure is retried wherever it happens, not at one caller.
    ///
    /// A dropped connection or a DNS blip is the server or the network shedding
    /// load; the next attempt usually succeeds. That retry was written once and
    /// wired into a single one of seven call sites, so every other path returned
    /// the error straight up and it reached the user as a 502. On one Mac the
    /// proxy log held 102 of them - 72 DNS lookups, 24 broken pipes - each one a
    /// blip that a second attempt would have covered.
    #[test]
    fn transient_transport_errors_are_worth_another_try() {
        assert!(worth_retrying("io: failed to lookup address information"));
        assert!(worth_retrying("io: Broken pipe (os error 32)"));
        assert!(worth_retrying("io: No route to host"));
        assert!(worth_retrying("io: unexpected end of file"));
        assert!(worth_retrying("timeout: global"));
        // A refusal from the server is an answer, not a blip - it must reach the
        // caller so the account logic can act on it.
        assert!(!worth_retrying("http status 401"));
        assert!(!worth_retrying("certificate verification failed"));
    }
}

#[cfg(test)]
mod wait_tests {
    use super::*;

    /// Every wait that could hang forever is bounded; the body is not.
    ///
    /// The agent had no timeouts at all. Every retry in this file fires on an
    /// ERROR, and an upstream that accepts and says nothing never produces one -
    /// so the proxy waited forever and the client waited with it, measured at
    /// two minutes with zero bytes and no log line at all.
    #[test]
    fn the_short_waits_are_bounded_and_the_body_is_not() {
        let (short, headers) = waits();
        assert!(short.as_secs() > 0 && short.as_secs() <= 30, "{short:?}");
        assert!(
            headers > short,
            "the response headers get more room than a connect: {headers:?} vs {short:?}"
        );
        // A streaming turn runs for as long as the model talks; bounding the
        // BODY would cut a working answer in half, which is the failure this
        // proxy exists to avoid. Nothing here may be small enough to do that.
        assert!(
            headers.as_secs() >= 120,
            "too tight for a slow first token: {headers:?}"
        );
    }

    /// The wiring, not the numbers: an upstream that accepts and never answers
    /// must produce an ERROR, because every retry in this file needs one.
    #[test]
    fn an_upstream_that_never_answers_becomes_an_error() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            let mut held = Vec::new();
            for c in listener.incoming() {
                // Accept and say nothing - the shape a black-holing network has.
                held.push(c);
            }
        });

        // On its own thread with a deadline. With no bound the call never
        // returns, and a test that HANGS on the defect it exists to catch
        // reports nothing at all.
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let agent = agent_with(Duration::from_millis(300), Duration::from_millis(300));
            let out = forward(
                &agent,
                "POST",
                &format!("http://127.0.0.1:{port}/v1/messages"),
                &[],
                b"{}",
            );
            let _ = tx.send(out.is_err());
        });
        match rx.recv_timeout(Duration::from_secs(20)) {
            Ok(is_err) => assert!(is_err, "silence must not read as success"),
            Err(_) => panic!("the relay never returned - the upstream wait is unbounded"),
        }
    }

    /// A spent budget is not worth spending again.
    #[test]
    fn a_response_timeout_is_terminal_but_a_connect_one_is_not() {
        assert!(
            !worth_retrying("timeout: receive response"),
            "four tries inside four is sixteen response budgets"
        );
        assert!(!worth_retrying("timeout: recv response"));
        // The short ones stay retryable: a route that flaps usually works next
        // time, and each attempt costs seconds rather than minutes.
        assert!(worth_retrying("timeout: connect"));
        assert!(worth_retrying("connection reset by peer"));
        // And the rules that were already here still hold.
        assert!(!worth_retrying("invalid certificate"));
    }
}
