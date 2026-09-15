//! Preserve the upstream response while removing connection-local metadata.

use super::{server::Request, skip_response_header, upstream::Upstream};
use std::io;
use tiny_http::{Header, Method, Response, StatusCode};

pub(super) fn respond(request: Request, upstream: Upstream) -> io::Result<()> {
    let connection_fields: Vec<&str> = upstream
        .headers
        .iter()
        .filter(|(name, _)| name.eq_ignore_ascii_case("connection"))
        .flat_map(|(_, value)| value.split(',').map(str::trim))
        .collect();
    let headers: Vec<Header> = upstream
        .headers
        .iter()
        .filter(|(name, _)| !skip_response_header(name))
        .filter(|(name, _)| {
            !connection_fields
                .iter()
                .any(|field| name.eq_ignore_ascii_case(field))
        })
        .filter_map(|(name, value)| Header::from_bytes(name.as_bytes(), value.as_bytes()).ok())
        .collect();
    let has_body = request.method() != &Method::Head
        && !matches!(upstream.status, 100..=199 | 204 | 205 | 304);
    if !has_body {
        // Do not drain a forbidden body; an upstream can keep it open forever.
        return request.respond(Response::new(
            StatusCode(upstream.status),
            headers,
            io::empty(),
            Some(0),
            None,
        ));
    }
    request.respond(Response::new(
        StatusCode(upstream.status),
        headers,
        upstream.reader,
        None,
        None,
    ))
}
