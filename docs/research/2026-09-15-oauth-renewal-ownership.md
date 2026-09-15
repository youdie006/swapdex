# OAuth renewal ownership: competitor source review

Reviewed on 2026-09-15. This is a source investigation and implementation
decision record, not a claim that the proposed behavior is released or that
an expired production account has recovered.

## Problem and required behavior

An account selected for inference can expire while the proxy is running.
In swapdex 0.160.0, a native CLI using the same account prevents the proxy
from refreshing it. That protection also blocks background keep-alive and
request-time renewal indefinitely when the native session stays open.
The request can then fall back to the client's own credential, potentially
charging a different account from the one selected in swapdex.

The required behavior is:

- Check the selected account before sending a request, and await renewal
  when its access token cannot serve that request.
- Concurrent callers share the renewal result, including failure; a recent
  attempt is not evidence that another caller successfully renewed it.
- Keep a still-valid access token usable after a transient early-renewal
  failure, but make the renewal failure visible.
- Do not implicitly use the client's different account when managed renewal
  fails. Explicit passthrough and separately enabled account failover are
  different policies and must retain their explicit meaning.
- Prevent competing refresh-token use by the proxy, manual commands,
  background jobs and native clients.

Access-token expiry is normal. Automatic renewal preserves continuity;
it does not disable provider expiry or guarantee that a revoked refresh
token can be recovered. Local `iat`, file mtime and `last_refresh` cannot
prove that an unobserved external holder has retired a token.

## Review scope

The following public revisions were inspected, including implementation
paths and selected regression tests. The competing applications were not
installed, executed or tested against live OAuth accounts.

| Project | Pinned revision | Relevant scope |
| --- | --- | --- |
| CC Switch | `42ac174dbc42e0cf50a50e60c5f2c3dcecca4560` | Managed Codex OAuth and live CLI credential reconciliation |
| CLIProxyAPI | `7bbfeaf8a7acf2cd5a834dcb0842539fe6aabc2b` | Request recovery, background refresh and Home delegation |
| TeamClaude | `9f6067437a3575326656c1c8d0f10e986f4bf8b8` | Claude/Codex account manager, forwarding and persistence |
| codex-auth-switcher | `433534e46a1f43c7fe09b3abe624b5f03fb4c585` | Manual profile renewal and usage-request recovery |

## Findings

### CC Switch: wait, reread, then reconcile generations

The Codex manager returns a cached token when it is sufficiently current.
Otherwise it acquires a per-account mutex, rereads managed native
`auth.json`, adopts a newer verified generation and checks its cache again
before spending a refresh token. If the server rejects a refresh token, it
rereads the native store and retries once only after adopting a different
token for the managed identity. Before applying a successful response, it
checks that the stored refresh token still matches the one sent [C1].

Different token material normally needs a strictly newer recorded timestamp;
equal timestamps and undated conflicting generations remain ambiguous.
An explicit server rejection enables a separate recovery path [C2].
The managed identity check includes the local account marker and user
identity; the upstream workspace ID alone is insufficient [C3].

The source explicitly describes synchronization back to native `auth.json`
as best effort: the native CLI does not share the manager's lock, and a
check followed by file replacement is not an atomic cross-process operation
[C3]. This is useful reconciliation, not proof that two holders cannot
spend the same refresh token concurrently.

Automatic failover has explicit prerequisites: proxy running, app takeover,
a configured queue and auto failover enabled [C4]. A selected account failing
does not by itself authorize adopting any available client credential.

### CLIProxyAPI: recover the same credential before failover

For an eligible 401, the manager attempts one request-level refresh of the
same credential before fallback. It locks by auth ID, rereads current state,
and reuses the winner's token when another request has already replaced the
failed bearer [P1]. That is stronger than suppressing all attempts for a
fixed time without waiting for a result.

Refresh errors retain separate state for an unexpired access token and an
expired/unavailable one. The code records the last error and schedules a
retry with backoff; a failed early refresh does not alone make an unexpired
access token unusable [P1]. The background scheduler caps timer waits to
detect expiry after a sleeping machine resumes [P2].

When Home control is enabled, both Claude and Codex executors delegate
refresh to Home. If Home is unavailable, they return an error rather than
performing a competing local refresh [P3]. This is an explicit ownership
boundary worth adopting.

There are limits to the analogy. The request-level one-recovery rule does
not mean exactly one HTTP attempt inside every provider executor. Also,
the inspected lifecycle path ignores the result of `persist`, even though
the persistence function uses generation ordering [P4]. Swapdex must not
equate an in-memory success with a durably saved rotated credential.

### TeamClaude: shared promise, guarded response, incomplete ownership

The actual inference forwarding path awaits `ensureTokenFresh`, whose normal
threshold is five minutes before expiry. Concurrent requests share the
account's `_refreshPromise`. Forced refreshes after a recent successful
refresh are suppressed to avoid repeatedly rotating for old 401 responses.
The response is discarded if a reload replaced the refresh token while
the exchange was in flight [T1]. The proxy has a bounded per-account 401
recovery path [T6].

This is not a complete cross-process solution. The file lock covers config
read-modify-write, not the OAuth exchange. After two seconds of contention
the writer proceeds without the lock [T3]. A rejected refresh token marks
the account unavailable, but transient failure is swallowed; forwarding
can continue with a potentially expired access token ([T1], [T2]).

`importFrom` is documented as reading credentials from the native file at
startup and reload. However, the refresh callback writes token fields to
the config row without an `importFrom` guard [T4]. This is a static
inconsistency, not a reproduced runtime bug. The dedicated save helper's
test does not establish that every callback preserves external ownership.

TeamClaude also distinguishes client-bound OAuth/session endpoints from
inference credentials [T5]. Swapdex should preserve that distinction without
introducing interception of unrelated authentication traffic.

### codex-auth-switcher: useful command-level checks

This smaller switcher renews a saved profile and applies the response only
if the snapshot is unchanged. Persistence failure is returned to the caller
[S1].
A usage-endpoint 401 triggers one refresh followed by another usage request
[S2]. These are useful command-level contracts, but do not establish
coordination with a long-running native CLI or proxy.

## Implications for swapdex

The current implementation was reviewed at
`4e41d25063a2a5cf44c94c19b950365d0f36aefe` ([W1], [W2]). Its 30-second
`RefreshGate` records attempts, not an in-flight operation with a shared
result. The claim precedes the native-holder check, so a skipped attempt can
also make another caller report that renewal is already happening. Separate
swapdex processes do not share that gate. Claude also lacks Codex's
post-response check against credential replacement [W1].

Do not remove the native-holder guard merely because the CLI points at the
local proxy. A base URL proves where inference travels, not who owns the
refresh token. A file lock only coordinates participants that honor it;
the reviewed competitors do not establish a universal native-client lock.

An additional upstream Codex source check makes the distinction concrete.
At revision `6b9826e3aa83b1a5947db50f4332cb9c65f1b340`, MCP startup calls
`AuthManager::auth()` [N1]. For locally managed ChatGPT credentials that
method can proactively refresh OAuth [N2]. Changing inference provider
authentication alone therefore does not prove that the CLI has relinquished
OAuth ownership across its other integrations. This source check is not a
claim that an isolated proxy-only launch has been validated end to end.

The preferred architecture is one renewal authority per credential chain,
plus request-time waiting. Account identity and credential-chain identity
must not be confused: two users may share a workspace, and one user may
have independently issued logins. Do not merge credentials by display name,
email or workspace ID alone.

| Ownership | Request behavior | Renewal authority |
| --- | --- | --- |
| Proxy owns the chain | Check, share a bounded renewal, then inject the selected credential | Proxy, with participating manual/background callers serialized through the same mechanism |
| Native client owns the chain | Reread the designated authoritative store; report unavailable if it has no usable token | Native client; proxy must not spend a copied refresh token |
| Existing ownership is ambiguous | Preserve a still-usable selected token; give an explicit actionable failure once unavailable | Do not infer a safe handoff from process environment or timestamps |

Credential reconciliation can help when a newer, correctly identified native
generation is already available. It cannot force an idle native owner to
refresh, recover an unobserved external generation, or retroactively remove
a token held in an existing process's memory. Existing sessions therefore
need an honest migration path; replacing the proxy executable alone cannot
prove that ownership has transferred.

A local coordination mechanism cannot fence an unknown credential copy on
another machine. External consumers must use the designated renewal owner;
the UI must report observed failures without claiming advance detection of
every remote token rotation.

## Implementation acceptance criteria

These are requirements for the authorized repair, not completed changes:

- A slow successful refresh plus concurrent turns sends one exchange, and
  all turns use the same resulting selected credential.
- A failed refresh shares the failure; it neither spends the same token in
  a burst nor claims that a non-running renewal is in progress.
- Participating processes hold coordination across read, exchange and
  persistence, and reread after acquiring it. A timeout must not bypass it.
- Credential replacement during success or rejection never overwrites or
  marks the replacement generation rejected.
- Request-time checks cover expiry after sleep and one bounded same-account
  recovery after a genuine credential 401, before any explicitly enabled
  failover. Do not replay a partially streamed inference response.
- A failed early renewal leaves usable access available and its failure
  visible; an expired selected credential produces a clear proxy error.
- Managed failure never injects the client's other login. Explicit
  `serve --off` and client-bound authentication routes retain passthrough.
- Native ownership remains protected until a supported, verified launch
  mode demonstrably removes the competing OAuth refresh path. Verify
  plugins, MCP, history/resume and vendor-specific features during migration.
- Successful exchange without successful durable persistence is an error,
  not a completed renewal. Diagnostics never include credential values.

## Validation and delivery state

The unmodified swapdex baseline passed `cargo test --all --locked`:
998 tests passed, one ignored. This verifies the existing baseline only;
some existing tests deliberately assert the passthrough behavior that the
repair must replace. Competitor conclusions above are static source findings.

This research does not change the installed 0.160.0 binary, account routing,
credentials, or running clients. Implementation, regression validation,
release and installation remain separate follow-up work and must be
recorded as such in the eventual fix PR and release record.

## Pinned sources

[C1]: https://github.com/farion1231/cc-switch/blob/42ac174dbc42e0cf50a50e60c5f2c3dcecca4560/src-tauri/src/proxy/providers/codex_oauth_auth.rs#L814-L1005
[C2]: https://github.com/farion1231/cc-switch/blob/42ac174dbc42e0cf50a50e60c5f2c3dcecca4560/src-tauri/src/proxy/providers/codex_oauth_auth.rs#L1198-L1305
[C3]: https://github.com/farion1231/cc-switch/blob/42ac174dbc42e0cf50a50e60c5f2c3dcecca4560/src-tauri/src/codex_config.rs#L840-L970
[C4]: https://github.com/farion1231/cc-switch/blob/42ac174dbc42e0cf50a50e60c5f2c3dcecca4560/docs/user-manual/en/4-proxy/4.3-failover.md#L11-L19
[P1]: https://github.com/router-for-me/CLIProxyAPI/blob/7bbfeaf8a7acf2cd5a834dcb0842539fe6aabc2b/sdk/cliproxy/auth/conductor_refresh.go#L466-L619
[P2]: https://github.com/router-for-me/CLIProxyAPI/blob/7bbfeaf8a7acf2cd5a834dcb0842539fe6aabc2b/sdk/cliproxy/auth/auto_refresh_loop.go#L13-L18
[P3]: https://github.com/router-for-me/CLIProxyAPI/blob/7bbfeaf8a7acf2cd5a834dcb0842539fe6aabc2b/internal/runtime/executor/helps/home_refresh.go#L90-L126
[P4]: https://github.com/router-for-me/CLIProxyAPI/blob/7bbfeaf8a7acf2cd5a834dcb0842539fe6aabc2b/sdk/cliproxy/auth/conductor_lifecycle.go#L148-L266
[T1]: https://github.com/KarpelesLab/teamclaude/blob/9f6067437a3575326656c1c8d0f10e986f4bf8b8/src/account-manager.js#L3620-L3730
[T2]: https://github.com/KarpelesLab/teamclaude/blob/9f6067437a3575326656c1c8d0f10e986f4bf8b8/src/server.js#L2117-L2122
[T3]: https://github.com/KarpelesLab/teamclaude/blob/9f6067437a3575326656c1c8d0f10e986f4bf8b8/src/config.js#L171-L279
[T4]: https://github.com/KarpelesLab/teamclaude/blob/9f6067437a3575326656c1c8d0f10e986f4bf8b8/src/index.js#L320-L352
[T5]: https://github.com/KarpelesLab/teamclaude/blob/9f6067437a3575326656c1c8d0f10e986f4bf8b8/docs/proxy-modes.md#L19-L27
[T6]: https://github.com/KarpelesLab/teamclaude/blob/9f6067437a3575326656c1c8d0f10e986f4bf8b8/src/server.js#L2503-L2510
[S1]: https://github.com/sasanktumpati/codex-auth-switcher/blob/433534e46a1f43c7fe09b3abe624b5f03fb4c585/internal/auth.go#L238-L265
[S2]: https://github.com/sasanktumpati/codex-auth-switcher/blob/433534e46a1f43c7fe09b3abe624b5f03fb4c585/internal/auth.go#L412-L444
[W1]: https://github.com/youdie006/swapdex/blob/4e41d25063a2a5cf44c94c19b950365d0f36aefe/src/refresh.rs#L173-L313
[W2]: https://github.com/youdie006/swapdex/blob/4e41d25063a2a5cf44c94c19b950365d0f36aefe/src/proxy/mod.rs#L1610-L1718
[N1]: https://github.com/openai/codex/blob/6b9826e3aa83b1a5947db50f4332cb9c65f1b340/codex-rs/core/src/session/session.rs#L959-L985
[N2]: https://github.com/openai/codex/blob/6b9826e3aa83b1a5947db50f4332cb9c65f1b340/codex-rs/login/src/auth/manager.rs#L2354-L2371
