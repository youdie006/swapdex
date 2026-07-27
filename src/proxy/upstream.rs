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
