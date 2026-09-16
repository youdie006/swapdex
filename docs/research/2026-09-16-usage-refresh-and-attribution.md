# Usage refresh and account attribution audit

Source baseline: `v0.165.5`, `4e2c8f552236dea7fc58f1ab7e109b906f244d03`.

## Two separate observations

The user reported an unexpected decrease in a nonselected account's remaining
quota and a WSL dashboard that appeared current only after reopening. These
must be investigated independently. An account-wide quota movement does not
identify the requesting client or prove a proxy routing error.

## Account attribution: read-only evidence

The audit queried owned usage endpoints without sending model requests or
renewing credentials. It extracted only process/routing metadata, response
rate-limit headers, and token-count metadata from recent native sessions.
It did not read private harness state or daily memory files. Account IDs,
credentials, conversations and customer project names are omitted here.

- WSL: 40 recent session files were scanned in full, beginning at
  2026-09-16 00:51 UTC. Six completed standalone `codex exec` sessions had
  16 main-limit token-count records whose weekly reset matched the
  nonselected account. They also recorded positive, increasing model token
  usage. Five ran during 01:49–01:59 UTC; the sixth ran during 06:39–06:40 UTC.
- WSL's proxy journal from 00:51 through 06:59 UTC recorded 3,368 successful
  model-response requests under the selected account and none under the
  nonselected account. Account labels alone are not wire credential evidence;
  the separate response-header audit also found selected-account reset times
  and no nonselected-account reset among retained native HTTP header records.
- The six continuing interactive WSL native sessions carried the local proxy
  base URL. This is not sufficient proof of effective routing: the separate
  CLI reproduction below shows that native parsing can discard that override.
  A bounded process observation from 07:10 through 07:14 UTC found
  no new standalone native job. Completed short jobs would be missed by a
  process snapshot, which is why session metadata was also checked.
- Windows: one recent native session contained 151 main-limit observations
  through 06:38 UTC, matching the selected account's weekly windows, allowing
  for a one-second reset conversion difference. None matched the other account.
- M3: the known default and registered native state databases contained no
  threads updated in the audited interval. This is limited to those homes;
  it is not evidence about every possible remote client.

Matching stock Codex source obtains session rate-limit snapshots from model
response headers/SSE, then records them with token usage. Native account quota
polling is a separate `/wham/usage` request using the native home's auth; that
read must not be mistaken for a model invocation or billed usage.

The managed proxy source replaces both Authorization and ChatGPT-Account-ID.
The review found no normal managed path retaining the original caller's bearer.
Explicit direct-provider overrides, native executable launches, passthrough
mode and other clients remain separate routes. The ended jobs' full launch
environment and outgoing request were not captured. The account-wide quota
decrease therefore cannot be attributed entirely to these jobs or this bug.

## Reproduced Codex configuration-scope bypass

A later process capture found a new image-review helper running `codex exec`
with its own `-c model_reasoning_effort=...`, image input and a stdin prompt.
Its native argv included Swapdex's proxy address before the `exec` subcommand.
The intermediate launcher process briefly appeared as `codex` before becoming
the Node wrapper; that transient process name was not proof of a direct job.

With stock Codex 0.154.0, synthetic credentials, and only loopback servers,
the same option pattern reproduced the bypass. An `openai_base_url` override
prepended before `exec` was discarded when `exec` had its own `-c` option.
The rejecting direct endpoint received model requests and the selected-account
proxy received none. Putting the managed override into the same argument scope
restored the selected-account route. No real model requests were sent.

The production fix tracks the last actual configuration option and inserts the
managed override immediately before it. It preserves original argument order,
quoted values and the `--` prompt boundary; a blind append would mishandle
that boundary. The stock-client fixture now covers caller configuration along
with profiles and explicit custom providers. New launches receive the fix;
already-running native processes retain their parsed launch configuration.

## Reproduced dashboard defect

The main list already refreshes local account state and asynchronously reads
quota. Usage (`u`) and Quota (`%`) detail panels instead set `pending` once,
run their child command on the UI event loop, and never schedule another read.
The main quota receiver also retained a disconnected channel indefinitely.

A PTY fixture against the installed `0.165.5` native executable seeded a fake
account and a fake curl that delayed a read by three seconds. After opening
Quota, Escape followed by quit could not exit within 1.5 seconds: the child
read blocked input. This was an expected regression failure, not a network
or credential failure. The fixture sent zero model requests and cleaned up
its own process group.

The panel fix uses an owned asynchronous child, refreshed on entry and every
45 seconds after completion. Leaving the panel stops and reaps that child and
its helper process group. A manual `r` refresh preserves the last content and
scroll position. Main-list disconnected readers recover without delaying a
newly changed account identity.

Final test, publication and target installation results must be recorded in
the PR/release after verification; this source note alone does not claim a
published or installed update.

## Candidate verification

The corrected PTY fixture reproduced blocked navigation on installed 0.165.5.
On the candidate, Escape/quit during a three-second quota read completed in
0.211 seconds, and no fixture reader remained running. Usage and Quota stayed
open, observed changed fixture data after 44.511 and 45.720 seconds respectively
(the cadence starts at each completed read), and accepted `r` to read another
change immediately. Captured screens showed the new values, freshness status
and refresh/back hints without clipping. The fixture sent zero model requests.

The first candidate PTY attempt failed at fixture setup: its fake curl emitted
a trailing newline after the HTTP status, unlike curl. Correcting the fake
response restored valid quota input; the unchanged old binary still failed
the navigation assertion, and the candidate passed. This was a test-fixture
defect, not a suppressed product failure.

The stock Codex loopback matrix passed 15 managed launch forms, including
root/subcommand config combinations, compact and long config flags, nested
`exec resume`, all four profile forms, a literal prompt after `--`, and image
input with a stdin prompt and an empty API-key environment variable. Two
custom-provider cases kept their own endpoint and synthetic key. Assertions
checked the selected bearer and account on actual model requests, an unchanged
launch auth file, and zero direct-endpoint model requests.

The npm checks passed all seven tests. Dependency auto-merge tests passed
all 22 cases from `.github/scripts`. An initial generic discovery command
pointed at `scripts` and ran zero tests (exit 5); the repository CI command
above was then used successfully.

Final Rust test, clippy, formatting and binary build checks passed after
the 0.165.6 metadata update. The final shim worker used the worktree-local
Cargo target rather than the earlier shared target. A man-page version guard
rejected the old shared 0.165.5 binary; generation and final behavioral checks
then used the version-checked worktree-local 0.165.6 executable. Generated
roff trailing whitespace was normalized before the final diff check.
