# Streaming interruption and issue #22 verification

## Candidate

Version: 0.165.3. Branch: `fix/stream-flush`. Base: `6f9e245` (0.165.2).
This record describes source verification. Publication and machine installation
must be verified and recorded separately in the version-specific release.

## Reproduced faults and corrections

- The installed 0.165.2 proxy withheld a flushed, small SSE event while a
  loopback upstream remained open. Gated Claude and Codex tests failed before
  the fix and now receive headers, heartbeat and completion bytes before EOF.
- The locked ureq 3.4.0 client applied its five-minute response-header deadline
  to a started response body. A deterministic 150 ms / 400 ms loopback test
  failed with `Timeout(RecvResponse)` and passed after updating to 3.4.2.
- tiny_http's public writer could not close a failed keep-alive response. A
  Hyper HTTP/1 listener now distinguishes EOF from failure, flushes available
  fragments and closes only the failed connection. A capacity-one channel
  connects it to the existing synchronous handler. CI limits Tokio/Hyper
  features and continues to reject system TLS and additional HTTP clients.
- Issue [#22](https://github.com/youdie006/swapdex/issues/22) required quota reads
  never to renew credentials. The 0.165.1 renewal-order fix did not meet that
  requirement. Expired slots, saved profiles and matching expired native
  logins now remain offline without OAuth, credential writes or expired-token
  usage requests. Seven fake-provider tests cover read-only behavior.
- Existing overage-only rejection, fractional threshold and Homebrew service
  upgrade regressions remain covered. Allowed plans with extra usage disabled
  remain available for selection and real failover.

## Verification evidence

- `cargo test --all --locked --jobs 2`: **1,213 passed, 0 failed, 2 ignored**.
- `cargo clippy --all-targets --locked --jobs 2 -- -D warnings`: passed.
- `cargo fmt --all -- --check`: passed.
- `cargo audit`: passed; 195 locked dependencies scanned.
- Python dependency merge gate: 22 tests passed. npm launcher/publish checks:
  seven tests passed.
- CI listener dependency limits and bundled-rustls-root checks: passed.
- Installed-binary account-routing verifier: passed, including Claude/Codex
  A–B–A payer changes and accepted-POST replay protection.
- Installed-binary streaming verifier: failed on exact installed 0.165.2 and
  passed on the 0.165.3 candidate for Claude and Codex. Headers, heartbeat and
  completion arrive before upstream EOF; a malformed upstream closes the
  persistent downstream without a successful terminal chunk. Each case sends
  exactly one upstream request. Linux and macOS CI run this verifier.
- Stock Codex session-resume verifier: passed, including WebSocket-to-HTTP
  fallback, native session listing/resume and preserved conversation bytes.
- First-use verifier in foreground mode: all six fresh/existing-login cases
  passed. Default autostart mode correctly refused because live services held
  ports 8787/8788; those services were not stopped for this fixture.

Initial streaming tests also exposed external usage calls in routing fixtures.
The proxy fixtures now default to a failing local curl stub unless a test
explicitly supplies its fake usage endpoint. A concurrent late-refusal test
failed once before that isolation, then passed alone and in the full suite.

## Scope and limits

The affected live WSL session repeatedly logged an interrupted API response;
proxy timeouts coincided with its failures. No prompts were replayed and no
native session was cancelled or edited during inspection. Fake-provider tests
prove the corrected transport and credential contracts; they do not prove that
all provider-side latency or every long-running native conversation is resolved.

An entirely silent synchronous upstream read can outlive a disconnected client
until that read returns. Other clients continue to work. Request-size and
admission limits retain the previous policy; this patch bounds response
queueing and removes the extra handler-side request copy.
