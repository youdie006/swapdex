# Persist Claude usage lookup backoff across callers

## Observed failure and authorized scope

The user reports that `endpoint busy` persists even after restarting Swapdex.
The 0.165.8 display change explained HTTP 429 but did not change request pacing.
The WSL picker repeatedly alternates between successful readings and limited
lookups. A single read-only diagnostic on 2026-09-17 returned HTTP 429,
`rate_limit_error`, and `Retry-After: 0`; it sent no OAuth or model request.

`quota::fetch_with_retry` sends four attempts approximately 0, 0.4, 1.3 and
3.1 seconds apart when every reply is 429. The picker polls every 45 seconds.
The managed proxy also uses this helper, with its own measurement schedule.
Neither callers nor newly started processes share a retry deadline. This
demonstrates unnecessary retry traffic; it does not establish the provider's
undocumented usage-endpoint budget or prove that Swapdex is its only consumer.

This continues the user's authorized bug fixes, installation and verification.
Credential renewal, account selection, model-request retries and software
self-update are outside this change. Existing conversations must remain open.

## Design

### One shared deadline per credential

Add a small Claude-usage coordination module under the existing `Paths`
store. Key its files by a SHA-256 fingerprint of the access credential, never
by the raw token. A separate per-key advisory file lock covers reading state,
one bounded HTTP lookup, and atomic state replacement. Different credentials
remain independent. Lock/state files use private permissions. The lock inode
stays stable; do not unlink it while other processes might be waiting.

Both the quota command (including picker subprocesses) and proxy measurement
pass their existing `Paths` to this coordinator. Replace the body of the old
four-attempt helper; its gated closure invokes the raw header-aware fetch once.
There must be no path from a permitted attempt back into the old retry loop.
A 429 produces one network
request, records the next permitted attempt and immediately returns the
existing throttled result. Further callers before the deadline return that
result without network traffic. Restarting cannot reset the deadline. A new
credential has an independent key so an old rejection does not poison it.

On repeated 429 replies, local fallback waits are 60, 120, 240, 480 and then
900 seconds. Repeated local reads during a wait neither extend the deadline
nor increment the failure count. Any received non-429 HTTP response clears
the failure history; a transport failure leaves its count intact but does not
extend the old deadline. Other HTTP/transport outcomes preserve classifications;
they must not become a login-expiry or spent-account verdict because of
usage lookup throttling. Do not retry other outcomes as part of this fix.

The JSON state schema is `{ "version": 1, "failures": N,
"throttled_at": T, "retry_at": R }`, with Unix-second timestamps.
`failures` is capped at five, representing the five fallback steps above.
Require `1 <= N <= 5`, checked timestamp arithmetic, and
`fallback(N) <= R - T <= 86400`. Reject a throttle timestamp more than
60 seconds ahead of the reader's wall clock as invalid state. A received
non-429 HTTP response removes only this JSON record under the held lock;
the separate lock file remains. An unsuccessful transport does not rewrite
the record. A new 429 atomically replaces the complete record with its
completion time, incremented/capped count and computed deadline.

### Preserve Retry-After information

The quota-specific curl request captures response headers into a private
temporary file, alongside the existing body/status transport. Preserve the
shared transport's `-q`, token-on-stdin discipline, socket safety, bounded
network timeouts and child reaping. Do not change OAuth, Codex or inference
transports. In particular, `run_curl_cfg` keeps its existing body/status API;
only a separate quota response wrapper captures/reads headers. Parse only the
final HTTP response header block, case-insensitively.

Use the larger of local fallback and a valid server delay. Accept delay-seconds
and HTTP-date using `httpdate`, already in the dependency graph. Empty,
malformed, negative, zero or elapsed values use local fallback; the observed
zero must not cause another immediate burst. Bound server delays to 24 hours
and use saturating arithmetic for hostile/overflowing values. This bound must
be documented and tested rather than described as unlimited header compliance.

### Failure handling and observation age

State I/O/lock failures report a lookup-coordination problem without touching
credentials or falling through to uncontrolled retries. Use a bounded lock
wait of 20 seconds; a process holding the lock cannot hang the picker
indefinitely. Atomic writes prevent partial valid state. Missing state means
the first attempt is allowed. Malformed JSON, an unknown schema or invalid
timestamps report a coordination error without a network request; do not
silently discard unreadable state. Stored deadlines must be consistent with
the recorded throttle time and the 24-hour maximum, using checked arithmetic.
Preserve the prior successful usage figures and their original observation
time on all failed/deferred lookups.

Represent these local failures as a distinct `Fetch::Coordination(String)`
variant, not `Offline`. Human output and the TUI say the usage lookup is
unavailable with the coordination reason. JSON adds `status: "unavailable"`
and `detail`; it leaves the top-level offline field null when coordination
alone failed. Preserve all existing HTTP/transport status values and meanings.
The proxy's no-number diagnostic also names lookup coordination failure.

Keep existing HTTP/transport JSON statuses and throttle wording. No new account-routing
policy, model inference, token refresh, or cross-machine synchronization is
introduced. Both machines independently obey backoff. A remote provider or
another client can still return 429 after our next permitted lookup.

## Alternatives considered

- A slower global dashboard timer would miss proxy/manual callers and reset
  on process restart.
- In-memory per-process backoff would still duplicate requests from another
  picker, quota subprocess or proxy.
- A persisted per-credential deadline directly covers the demonstrated
  failure without changing healthy account selection or model traffic.

## Verification and delivery

Before implementation, reproduce the repeated-request count against the
installed 0.165.8 binary with synthetic credentials and a fake curl. Add a
failing integration regression for sequential command invocations and one
for concurrent invocations. Validate fallback escalation, header parsing,
expiry recovery, independent credentials, success reset, corrupt/blocked
state, private persistence, and unchanged credentials/cache timestamps.
Include 401/403, malformed 2xx and other non-429 HTTP resets, plus transport
failure followed by another 429 retaining the prior count. Failed/deferred
lookups must leave prior cache figures and `at` byte-identical; only a fresh
successful response can replace those figures and record the current lookup's
observation time, retaining the command's existing timestamp convention.
Every fake-curl integration fixture sets both `SWAPDEX_ROOT` and
`SWAPDEX_CURL`, and proves its expected fake calls were used.

Exercise the final installed candidate in a real PTY: limited lookup, cached
age retained, recovery after deadline in the same open picker. Use fixtures
with no external OAuth/model calls. Run Rust tests, fmt, Clippy, dependency
constraints, npm/dependency-gate tests and appropriate installed first-use,
account-routing and streaming checks. Review the code independently.

Record concrete behavior in the changelog. Commit/push reviewed work, publish
version-specific artifacts, install on WSL and M3, and verify actual service
executables. Record channel hashes, running versions and validation limits in
the release/PR. A picker still using older program code must be reported
separately from installed packages and updated services; never bypass cmux's
SSH control restriction or terminate a native conversation to reload it.

## Protocol references

- [HTTP Retry-After semantics](https://www.rfc-editor.org/rfc/rfc9110.html#name-retry-after).
- [curl header capture](https://curl.se/docs/manpage.html#-D).
- [Anthropic rate-limit guidance](https://platform.claude.com/docs/en/api/rate-limits)
  explains Retry-After for documented APIs; it is not a published request
  budget for the OAuth usage endpoint diagnosed here.
