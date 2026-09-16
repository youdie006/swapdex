# Forward small streaming responses immediately

## Problem and evidence

The installed 0.165.2 proxy relays bodies through tiny_http 0.12. Its chunked
encoder buffers 8 KiB, and its socket writer buffers another 1 KiB. A loopback
upstream flushed an SSE ping and kept the response open; the installed proxy
delivered zero bytes during the client's one-second observation. The ping and
terminal event arrived only after the upstream finished. Exactly one upstream
request was made, using an isolated home and no real credentials.

One existing WSL Claude session repeatedly recorded `The response stopped
arriving`; another active session was making progress and had already
automatically compacted an earlier context overflow. The affected session's
latest failures coincided with proxy `timeout: receive response` diagnostics.
These observations do not establish that all provider latency has the same cause.

The lockfile also resolved ureq 3.4.0. That version incorrectly inherited the
response-header timeout during body reads, yielding the same `receive response`
error after the configured five minutes. A loopback test sends headers and a
first byte, then delays a second byte beyond a 150 ms header budget. It fails
with 3.4.0 and succeeds with 3.4.2. The manifest now requires the fixed version;
connection and header waits remain bounded without a streaming-body deadline.

## Requirements and approach

- Flush SSE headers before waiting for the first body bytes, and each available
  fragment including small pings and the final completion event.
- Preserve status, applicable headers, body bytes, authentication and selection.
- Keep ordinary responses, HEAD and bodyless-status semantics intact.
- Do not replay accepted POSTs or synthesize model events.

Use a Hyper HTTP/1 listener with a single-thread Tokio runtime, while retaining
the synchronous account selection and ureq/rustls upstream client. A one-fragment
channel bounds queued response data; explicit EOF/error events preserve the
distinction between a complete and interrupted response. Give the HTTP writer a
flush point before the first body read and between available fragments.

This narrowly replaces the former no-runtime dependency rule: CI now forbids
unrelated runtime features, additional HTTP client frameworks and system TLS.
It adds eleven locked dependencies. tiny_http remains for existing header and
response types and test fixtures, but no longer owns the proxy listener.

The direct tiny_http writer fixed buffering but could not close one failed
keep-alive stream: its public writer only exposes Write, and dropping it
releases the next response on the same socket. Neither a response Connection
header nor its upgrade API supplies shutdown. The new listener avoids a fork
or private socket access. Gated tests must prove prompt delivery, healthy reuse
and failed-stream closure for both tool protocols. Increasing timeouts or
padding model events does not fix bytes already withheld by a response buffer.

HTTP/1.0 unknown-length responses use connection-close framing. HTTP/1.1
responses retain ordinary length/chunk framing and healthy keep-alive.

## Existing limits retained

The synchronous upstream reader cannot be interrupted while an upstream is
fully silent after the client disconnects. Its handler can remain until that
read returns; tests verify that other clients continue and explicitly release
and join the held test producer. This change does not impose an idle body timer.
Request bodies are still read in full and admission remains unbounded, as in the
previous listener/handler. The collected buffer is moved into the account
handler, rather than keeping another full copy there. Response queueing is
bounded to one fragment. New request-size or concurrency policy is out of scope.

## Related issue #22

The existing overage, fractional threshold and stable Homebrew path fixes must
be verified. Quota's continued credential renewal does not satisfy the issue's
read-only requirement: expired slots must be reported without an OAuth exchange
or credential write, while proxy-owned renewal remains available for requests.
