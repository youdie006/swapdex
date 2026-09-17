# Claude usage lookup retry amplification

## Observations

The user reported that the dashboard's `endpoint busy` condition remained
after restart. Release 0.165.8 changed the wording to identify the failed usage
lookup; it did not change the retry policy. The new wording must not be
described as eliminating provider throttling.

On 2026-09-17 at 04:26 UTC, one authorized read-only usage lookup on WSL
returned HTTP 429, error type `rate_limit_error`, and `Retry-After: 0`.
It sent no OAuth or model request. No raw credential, account identifier or
response containing private usage figures is recorded here.

The installed 0.165.8 native binary had SHA-256
`cd2a84cc04dc0f438a777d0cf205c16c33f0260ab77fe8579151cce060fe28c9`.
Against one isolated fixture credential and a fake curl returning 429:

| Scenario | Processes | Outbound fixture calls |
|---|---:|---:|
| Sequential `quota --json` invocations | 2 | 8 in 6.23 seconds |
| Concurrent `quota --json` invocations | 2 | 8 |

Credential bytes were unchanged. All responses were synthetic and zero real
network, OAuth or model requests were sent by these reproduction runs.

The source independently explains the result: three waits of 400, 900 and
1,800 milliseconds precede a fourth attempt. The same helper is used by the
quota command and managed proxy. The picker starts periodic quota subprocesses,
so a purely process-local backoff would not survive normal operation.

## Implemented correction

Follow the [design](../superpowers/specs/2026-09-17-usage-endpoint-backoff-design.md)
and [implementation plan](../superpowers/plans/2026-09-17-usage-endpoint-backoff.md).
Persist a retry deadline per credential, preserve Retry-After, and coordinate
the quota command and proxy through it. Keep cached usage observation times
and existing authentication/account-routing semantics intact.

The base source at `0befa438d0650dd0e24f9d05de4ac6657ffbc1ce` passed
`cargo test --locked --lib quota`: 39 passed, zero failures.
The original sequential and concurrent process regressions failed with eight
calls instead of one. Both pass with the shared deadline. A separate regression
also caught a curl status of zero incorrectly clearing previous throttle
history; zero now retains history as a transport failure.

The isolated picker check is reproducible with
`python3 -B scripts/verify-usage-backoff.py --swapdex /absolute/native/binary`.
It requires tmux and creates its own private terminal server. The published
0.165.8 binary failed because the first throttled lookup made four requests.
The 0.165.9 candidate passed:

| Scenario | Result |
|---|---|
| Initial 429 with `Retry-After: 0` | One request and a 60-second deadline |
| Two further concurrent quota processes | Zero additional requests |
| Deferred usage reads | Credential and cached observation bytes unchanged |
| Fixture deadline elapsed; next response 200 | Same picker process automatically recovered |
| Successful response | Fresh usage timestamp; throttle history cleared |

The fixture adjusts only its synthetic deadline to avoid a real minute-long
wait, then waits for the picker's normal automatic refresh. It sends zero real
network, OAuth or model requests. First-use setup, account routing and streamed
responses also passed against the candidate.

Local full verification on 0.165.9 passed: 1,292 Rust tests, zero failures and
two existing ignored tests; Clippy with warnings denied; format and diff
checks; seven npm tests; 22 dependency-gate Python tests; bounded runtime/TLS
dependency checks; and `cargo audit`. The PTY result above is independent of
the unit and command-process regressions.

Publication channel integrity, release commit, installed hashes and actual
running-service checks are recorded in the version-specific
[v0.165.9 release](https://github.com/youdie006/swapdex/releases/tag/v0.165.9)
after publication. A source update alone does not establish installation.

## An already-open picker

On M3 the older renderer remained mapped to its previous executable inode,
but an observed automatic quota child mapped to the current installed native
binary. Updating that binary can therefore apply the request fix to this open
picker even while its renderer still uses the older wording. Terminal control
through the SSH-launched cmux client was denied by cmux's origin restriction;
no alternate injection route was used. This observation does not establish
that every old launcher on every platform resolves children the same way.

## Limits of the evidence

The observed reply and retry amplification prove avoidable local requests.
They do not reveal the OAuth usage endpoint's unpublished limit, its scope,
or traffic from other machines/clients. Respecting a retry deadline reduces
Swapdex's contribution; the provider can still limit a later request.

HTTP Retry-After accepts delay-seconds and HTTP-date according to
[RFC 9110](https://www.rfc-editor.org/rfc/rfc9110.html#name-retry-after).
[curl's header capture](https://curl.se/docs/manpage.html#-D) allows inspecting
this metadata without changing the shared body/status transport.
