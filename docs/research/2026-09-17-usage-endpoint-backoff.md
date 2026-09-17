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

## Intended correction

Follow the [design](../superpowers/specs/2026-09-17-usage-endpoint-backoff-design.md)
and [implementation plan](../superpowers/plans/2026-09-17-usage-endpoint-backoff.md).
Persist a retry deadline per credential, preserve Retry-After, and coordinate
the quota command and proxy through it. Keep cached usage observation times
and existing authentication/account-routing semantics intact.

The base source at `0befa438d0650dd0e24f9d05de4ac6657ffbc1ce` passed
`cargo test --locked --lib quota`: 39 passed, zero failures.
Implementation, regression, publication and installed-runtime results will be
recorded with the completed change and its version-specific release/PR.

## Limits of the evidence

The observed reply and retry amplification prove avoidable local requests.
They do not reveal the OAuth usage endpoint's unpublished limit, its scope,
or traffic from other machines/clients. Respecting a retry deadline reduces
Swapdex's contribution; the provider can still limit a later request.

HTTP Retry-After accepts delay-seconds and HTTP-date according to
[RFC 9110](https://www.rfc-editor.org/rfc/rfc9110.html#name-retry-after).
[curl's header capture](https://curl.se/docs/manpage.html#-D) allows inspecting
this metadata without changing the shared body/status transport.
