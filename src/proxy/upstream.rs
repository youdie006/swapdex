//! The upstream leg: forward one request to the API and hand back a streaming
//! reader. Synchronous by design - one thread owns one request end to end, so a
//! client disconnect surfaces as a write error and drops the upstream read with
//! it (no separate cancellation machinery).

use anyhow::{Context, Result};
use std::io::Read;

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
pub fn agent() -> ureq::Agent {
    ureq::Agent::config_builder()
        .http_status_as_error(false)
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
pub fn forward(
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
