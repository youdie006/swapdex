# Codex account switching: expanded source survey

Reviewed 2026-09-15. The requirement is to keep multiple existing Codex sessions
running while making account selection and renewal behavior predictable. This
survey extends the [renewal ownership review](2026-09-15-oauth-renewal-ownership.md).
It is source evidence, not an installation recommendation or a claim that the
competitors' tests passed locally.

## Discovery and selection

Five GitHub repository searches returned 227 unique candidates: `codex account
switcher` (60 results), `codex multi account` (60), `codex switch` (70, sorted by
stars), `codex proxy` (60, sorted by stars), and `codex oauth proxy` (40). These
are search limits, not total matching repository counts. The [candidate index](2026-09-15-codex-switcher-discovery.json)
records actual returned results and their originating searches. GitHub search
is bounded and ranking-dependent; this is not every public implementation.

Provider URL editors, VPN launchers, unrelated model adapters and redundant
forks were screened out. Detailed review prioritizes executable account-switch
paths, refresh coordination, native credential ownership, failure recovery,
protocol continuity and regression tests. Stars are discovery metadata rather
than evidence of correctness. Public source was read at pinned commits; no
competitor was installed or run against a live account.

## Most applicable mechanisms

| Implementation | Mechanism worth adapting | Limit that matters for Swapdex |
| --- | --- | --- |
| CC Switch | Lock, reread native auth, verify identity, guard a late refresh response | Its own comments acknowledge the native CLI does not share its lock; timestamps do not establish exclusive ownership |
| CLIProxyAPI | Bounded same-account 401 recovery, wait for another caller's result, explicit central refresh delegation | Persistence and per-executor behavior need separate verification; see the earlier review |
| VallierDev/codex-switcher | Token/account-header binding, explicit hard routes, centralized renewal, account-sensitive continuation handling | Some affinity/401 and established-WebSocket bookkeeping uses global selection rather than the request's bound account |
| xjoker/codex-switch | OS file locks, ordered launch/auth transactions, compare-and-swap persistence, retain rotated credentials even if later usage lookup fails | Its `use` path replaces a native auth file; it does not establish next-request switching for existing processes |
| zyycn/codex-proxy-rs | Revision-checked credential persistence, distinct transient/stale/lease outcomes, account-and-conversation WebSocket pool keys | Immediate inference-401 renewal was not found in the OpenAI adapter; optional recovery logging includes raw tokens and must not be copied |
| mehdic/codex-proxy | Delegate OAuth entirely to official `codex app-server`; serialize sticky conversations | This starts agent threads behind an API adapter; it is not a transparent drop-in transport for existing Codex sessions |

### Native file switchers have a different continuity contract

Lampese/codex-switcher acquires an application auth mutex, refuses a switch
while Codex is running, saves the latest native credential before replacing
`auth.json`, then updates its active marker [L1]. That reconciliation addresses
stale saved copies, but its running-process guard means it does not meet our
requirement of retaining all live sessions.

Its refresh path rereads live auth under that application lock and leaves
active credentials to a running Codex [L2]. The file writer uses `fs::write`
followed by permissions, and synthesizes `last_refresh = now` when writing a
saved profile [L3]. Neither an application mutex nor a new timestamp proves
that an independently running native client relinquished a refresh token.
The checked tree and GitHub metadata did not declare a license; learn from the
behavior rather than incorporating its source.

xjoker/codex-switch is a stronger reference for local file transactions. Its
OS locks have a timeout that returns an error instead of replacing a live lock
file [X1]. Profile refresh persistence checks the presented refresh token while
holding the transaction, and updates the live copy only if that copy still
holds the same presented token [X2]. Its usage path preserves a successfully
rotated token before handling a failed usage response [X3]. These protect
participating writes; the exchange occurs outside that persistence transaction,
so they do not by themselves exclude concurrent OAuth consumers.

Five further file/desktop implementations were inspected. Each of these five
declares MIT licensing in its pinned tree. Their useful mechanisms and tested
contracts differ from keeping existing sessions running through a proxy:

| Repository | Source behavior and evidence | Continuity / verification limit |
| --- | --- | --- |
| lordydord/Codex-Account-Switcher | Runs `codex-auth switch`, verifies the active email up to three times, attempts rollback on verification failure; records a separate relaunch failure [D1] | Verification reads the helper's active marker; then Desktop is relaunched. Infrastructure tests cover cache retention, pruning and process timeouts [D2], not universal next-turn switching |
| liuzhao1225/codex-account-switcher | Closes Desktop, verifies current identity, saves it, activates and verifies target, commits the marker and reopens; failures in target verification/commit restore the original [D3] | Desktop lifecycle is part of the contract. Its usage UI explicitly keeps a stale last-known-good value after lookup failure [D4] |
| bourbaki-lab/codex-account-switcher | Uses an OS lock, process preflight, normal Desktop quit, credential backup, verified replacement, session-file comparison and rollback [D5] | Requires quiescence. Its isolated App Server usage probe preserves refreshed auth even if quota lookup fails [D6], but copying a rotating credential still requires exclusive ownership |
| jesse-merhi/cxa | Reconciles live auth under a store lock before switching; prints restart guidance [D7]. An isolated App Server lookup saves updated auth before returning its usage result [D8] | The test explicitly allows switching while Codex runs and checks restart guidance, not adoption by an already-running session [D9] |
| Sls0n/codex-account-switcher | Saves auth snapshots and selects by symlink on Unix or copy on Windows [D10] | No tracked tests were found; package scripts provide a TypeScript build. A successful file/symlink change does not verify live-client adoption |

### Proxy selection includes connection state

VallierDev binds the selected bearer and its workspace header together, and
has separate treatment for explicit session routes [V1]. It also strips a
prior account's `x-codex-turn-state` when preparing an account switch [V2].
This is relevant because the official Codex client treats that header as
per-turn sticky routing state [N1]. Replacing a bearer is not enough evidence
that every continuation transport supports an account change safely.

Its central-owner guard is particularly useful: in `client`/`solo` mode, a
failed server refresh returns an error before the local OAuth path [V3]. Some
comments and log messages above that guard still describe local fallback;
the executable branch, not those comments, establishes the current behavior.
The same method ignores some store-save errors, so it is not a complete model
for durable success reporting.

Its account binding is not consistent across every recovery path. Learned
affinity can choose a request account without changing `store.current`, while
the non-hard-route 401 handler reads and refreshes that global selection [V4].
Similarly, a WebSocket handshake binds bearer/workspace but later usage, ban
and affinity handling consults the current global account [V5]. These static
paths are reasons to carry the request's account identity through recovery and
bookkeeping; their runtime failure modes were not exercised here.

zyycn's WebSocket pool key includes account, conversation, downstream
connection and egress identity [Z1]. Its credential writer checks the expected
revision and returns a distinct stale outcome on conflict [Z2]. Its scheduled
refresh tests include keeping refresh independent of quota exhaustion,
preserving transient error messages, clearing them on success, and deriving
expiry from the newly returned access token [Z3]. Those are useful regression
categories. Its optional recovery event deliberately records raw access and
refresh tokens [Z4]; that behavior conflicts with Swapdex's diagnostic contract.
Busy renewal leases return without sharing the leader's outcome [Z5]. Although
the gateway has same-account recovery flags, no construction of those flags was
found in the pinned OpenAI adapter's inference-failure mapping [Z6]. Scheduled
refresh and CAS persistence are therefore stronger references than its immediate
unexpected-401 recovery.

icebear0828/codex-proxy has extensive connection/session handling and restores
an owned full request before dropping an implicit rejected continuation [I1].
That qualification matters: deleting a continuation ID without possessing the
missing input can lose context. Its refresh lock is an exclusive-create file
that is broken after five minutes, and its scheduler skips rather than waits
when the lock is held [I2]. This is not a stronger replacement for an OS lock
and shared result. The scheduler also retries within that lock, so a
wall-clock stale-lock policy deserves particular scrutiny [I3]. The separate
`probeAccount` path directly refreshes after an in-process busy check, without
acquiring that file lock [I4]. Reactive 401 handling starts renewal without
awaiting it and moves to another account [I5]. Those are paths to avoid when
the user's selected account should first get a bounded recovery attempt.

Its explicit WebSocket continuations have useful stronger checks: they map a
response to its physical socket, verify the account, and reject missing, busy,
dead or mismatched owners [I6]. Its regular pool key includes account,
conversation and request variant, but not egress identity [I7]. The checked tree
contains a custom non-commercial `LICENCE`; GitHub reports `NOASSERTION` [I8].

### Official ownership is useful but changes the integration

mehdic/codex-proxy spawns `codex app-server --listen stdio://` and inherits the
process environment [M1]. That delegates OAuth to Codex. Its optional sticky
session pool preserves an app-server worker and thread and serializes requests
for the same session; it deliberately avoids another worker on sticky failure
[M2]. Adopting this would change request execution, tools and history semantics,
so it requires a separate compatibility design rather than a quick proxy patch.

Official Codex also caches authentication. Its unauthorized recovery reloads
only when the expected account matches, then attempts OAuth renewal [N2]. Its
proactive check uses access-token expiry when available, otherwise
`last_refresh` [N3]. Therefore replacing `auth.json` does not establish that all
existing processes will switch accounts on their next human message.

## Decisions for the current repair

- Keep existing native sessions and account selection intact. Desktop shutdown
  workflows are comparison material, not the default solution to this report.
- Use a read-only access snapshot from an actual native process only after
  verifying the selected identity. Do not adopt or spend its refresh token.
- Coordinate participating Swapdex renewals across read, exchange and durable
  save. Followers share an actual result; input and output credential generations
  bound cached success so an unrelated replacement cannot inherit it.
- Keep usable access separate from renewal health. Verified native ownership
  is different from an unverified blocked renewal or a recorded rejection.
- Return an explicit managed-auth error instead of silently using the client's
  other account. Preserve explicitly selected passthrough and account pinning.
- On a genuine HTTP 401, try bounded recovery for the same selected account
  before configured failover: reread a native-owned access snapshot or await
  coordinated managed renewal, and retry only with a changed usable bearer.
- Verify HTTP request switching separately from WebSocket/continuation
  behavior. Do not promise a universal next-human-turn switch from a file write
  or an HTTP-only test. Any future continuation repair must preserve the full
  request and avoid replaying a response after streaming has begun.
- Retain source links, test evidence and installation records separately. A
  research commit does not update a running proxy.

## Related operational reports

These issue bodies were read on the review date. They corroborate failure
classes; they are reporters' observations, not independently reproduced defects
in the revisions above. An issue being closed does not by itself prove a fix.

- [CC Switch #4474](https://github.com/farion1231/cc-switch/issues/4474)
  describes a current managed OAuth store diverging from a stale provider row,
  producing a false session-expired card even while the actual account works.
  [#3479](https://github.com/farion1231/cc-switch/issues/3479) separately reports
  a stale macOS Codex Keychain item shadowing a current file-backed login.
- [CLIProxyAPI #3783](https://github.com/router-for-me/CLIProxyAPI/issues/3783)
  describes overlapping refreshes spending one rotating token; it explicitly
  distinguishes in-process deduplication from multiple independent processes.
  [#1999](https://github.com/router-for-me/CLIProxyAPI/issues/1999) reports
  renewal rejection state disappearing across restart.
- [CLIProxyAPI #5095](https://github.com/router-for-me/CLIProxyAPI/issues/5095)
  reports premature exclusion after early-refresh failure while access remains
  usable. It supports displaying access availability and renewal health as
  separate facts.
- [Codex #27601](https://github.com/openai/codex/issues/27601) reports Desktop
  live-state trouble after account changes despite intact local history. This
  reinforces the need to verify actual running-client behavior separately from
  successful writes to a credential file.

## Pinned source evidence

[L1]: https://github.com/Lampese/codex-switcher/blob/839f2882f3a756fac074d483c39e7d2ecd582fd5/src-tauri/src/commands/account.rs#L135-L170
[L2]: https://github.com/Lampese/codex-switcher/blob/839f2882f3a756fac074d483c39e7d2ecd582fd5/src-tauri/src/auth/token_refresh.rs#L49-L97
[L3]: https://github.com/Lampese/codex-switcher/blob/839f2882f3a756fac074d483c39e7d2ecd582fd5/src-tauri/src/auth/switcher.rs#L29-L80
[X1]: https://github.com/xjoker/codex-switch/blob/a3392f6155f137149f44cd3a81337d35ec6739b5/src/profile.rs#L93-L170
[X2]: https://github.com/xjoker/codex-switch/blob/a3392f6155f137149f44cd3a81337d35ec6739b5/src/profile.rs#L289-L320
[X3]: https://github.com/xjoker/codex-switch/blob/a3392f6155f137149f44cd3a81337d35ec6739b5/src/usage/api.rs#L410-L440
[V1]: https://github.com/VallierDev/codex-switcher/blob/f64a44a81b9ae8354d76bcdff0e77ab23c8790e1/src-tauri/src/proxy.rs#L4445-L4514
[V2]: https://github.com/VallierDev/codex-switcher/blob/f64a44a81b9ae8354d76bcdff0e77ab23c8790e1/src-tauri/src/proxy.rs#L4434-L4520
[V3]: https://github.com/VallierDev/codex-switcher/blob/f64a44a81b9ae8354d76bcdff0e77ab23c8790e1/src-tauri/src/proxy.rs#L124-L219
[Z1]: https://github.com/zyycn/codex-proxy-rs/blob/24c236a7076db58cbdc7ad79d277923d616bc3ea/backend/crates/providers/openai/src/transport/websocket/pool/state.rs#L20-L95
[Z2]: https://github.com/zyycn/codex-proxy-rs/blob/24c236a7076db58cbdc7ad79d277923d616bc3ea/backend/crates/providers/openai/src/credential/repository.rs#L36-L80
[Z3]: https://github.com/zyycn/codex-proxy-rs/blob/24c236a7076db58cbdc7ad79d277923d616bc3ea/backend/crates/providers/openai/tests/credential/refresh.rs#L301-L672
[Z4]: https://github.com/zyycn/codex-proxy-rs/blob/24c236a7076db58cbdc7ad79d277923d616bc3ea/backend/crates/providers/openai/src/credential/recovery_log.rs#L36-L60
[I1]: https://github.com/icebear0828/codex-proxy/blob/501f6956dbdbacbd9d380755c781827102c449e1/src/routes/shared/proxy-retry-recovery.ts#L105-L134
[I2]: https://github.com/icebear0828/codex-proxy/blob/501f6956dbdbacbd9d380755c781827102c449e1/src/auth/refresh-lock.ts#L26-L66
[M1]: https://github.com/mehdic/codex-proxy/blob/da828dafa0bb98a932e022edb608e6b35f0a8d9b/src/subprocess/manager.ts#L94-L116
[M2]: https://github.com/mehdic/codex-proxy/blob/da828dafa0bb98a932e022edb608e6b35f0a8d9b/src/subprocess/session-pool.ts#L107-L169
[N1]: https://github.com/openai/codex/blob/6b9826e3aa83b1a5947db50f4332cb9c65f1b340/codex-rs/core/src/client.rs#L273-L292
[N2]: https://github.com/openai/codex/blob/6b9826e3aa83b1a5947db50f4332cb9c65f1b340/codex-rs/login/src/auth/manager.rs#L1842-L1854
[N3]: https://github.com/openai/codex/blob/6b9826e3aa83b1a5947db50f4332cb9c65f1b340/codex-rs/login/src/auth/manager.rs#L2945-L2967
[D1]: https://github.com/lordydord/Codex-Account-Switcher/blob/b223b24b05398f51a5194d253df2ba3aff05af81/Sources/main.swift#L4430-L4505
[D2]: https://github.com/lordydord/Codex-Account-Switcher/blob/b223b24b05398f51a5194d253df2ba3aff05af81/Tests/InfrastructureTests.swift#L8-L106
[D3]: https://github.com/liuzhao1225/codex-account-switcher/blob/5f2a0352d33a26b479bbe614b9b80843f4c9cb16/Sources/SwitcherCore/SwitchService.swift#L27-L137
[D4]: https://github.com/liuzhao1225/codex-account-switcher/blob/5f2a0352d33a26b479bbe614b9b80843f4c9cb16/Sources/SwitcherCore/AccountController.swift#L129-L170
[D5]: https://github.com/bourbaki-lab/codex-account-switcher/blob/4eb47a879b18200635a80c7d90831600df461725/Sources/CodexAccountSwitcherCore/AccountSwitchCoordinator.swift#L55-L213
[D6]: https://github.com/bourbaki-lab/codex-account-switcher/blob/4eb47a879b18200635a80c7d90831600df461725/Sources/CodexAccountSwitcherCore/ProfileRateLimitProbe.swift#L25-L104
[D7]: https://github.com/jesse-merhi/cxa/blob/a7332b0db2f3b43bc05f1d959d7a54c17c11ca07/src/cli.rs#L439-L460
[D8]: https://github.com/jesse-merhi/cxa/blob/a7332b0db2f3b43bc05f1d959d7a54c17c11ca07/src/app_server.rs#L94-L129
[D9]: https://github.com/jesse-merhi/cxa/blob/a7332b0db2f3b43bc05f1d959d7a54c17c11ca07/tests/cxa_cli.rs#L391-L440
[D10]: https://github.com/Sls0n/codex-account-switcher/blob/fad1a4199d448ed9dee7661eab3769aabb15235f/src/lib/accounts/account-service.ts#L26-L72
[V4]: https://github.com/VallierDev/codex-switcher/blob/f64a44a81b9ae8354d76bcdff0e77ab23c8790e1/src-tauri/src/proxy.rs#L3332-L3509
[V5]: https://github.com/VallierDev/codex-switcher/blob/f64a44a81b9ae8354d76bcdff0e77ab23c8790e1/src-tauri/src/proxy.rs#L7250-L7334
[Z5]: https://github.com/zyycn/codex-proxy-rs/blob/24c236a7076db58cbdc7ad79d277923d616bc3ea/backend/crates/providers/openai/src/credential/refresh.rs#L252-L285
[Z6]: https://github.com/zyycn/codex-proxy-rs/blob/24c236a7076db58cbdc7ad79d277923d616bc3ea/backend/crates/providers/openai/src/provider/failure.rs#L1165-L1333
[I3]: https://github.com/icebear0828/codex-proxy/blob/501f6956dbdbacbd9d380755c781827102c449e1/src/auth/refresh-scheduler.ts#L225-L317
[I4]: https://github.com/icebear0828/codex-proxy/blob/501f6956dbdbacbd9d380755c781827102c449e1/src/auth/health-check.ts#L40-L75
[I5]: https://github.com/icebear0828/codex-proxy/blob/501f6956dbdbacbd9d380755c781827102c449e1/src/index.ts#L113-L120
[I6]: https://github.com/icebear0828/codex-proxy/blob/501f6956dbdbacbd9d380755c781827102c449e1/src/proxy/ws-pool.ts#L652-L799
[I7]: https://github.com/icebear0828/codex-proxy/blob/501f6956dbdbacbd9d380755c781827102c449e1/src/routes/shared/proxy-ws-context.ts#L31-L51
[I8]: https://github.com/icebear0828/codex-proxy/blob/501f6956dbdbacbd9d380755c781827102c449e1/LICENCE#L1-L20
