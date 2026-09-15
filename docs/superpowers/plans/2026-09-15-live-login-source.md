# Live login source and renewal coordination repair

Approved behavior: use the selected account's actual usable login, await a
shared renewal when necessary, and never fall back implicitly to another
client account. The user explicitly requested repair after the source review.

## Reproduced local cause

On WSL, a Claude slot's saved credential is expired, while the same account
and organization have a current credential in the native default store used
by running Claude sessions. Reading only the saved slot reports expired and
blocks refresh because those native sessions own the account. No live OAuth
exchange, credential copying or account switch was used to establish this.

The user also reports the same account under another profile name on M3.
Check native default Keychain provenance there as well as file-backed WSL
stores; do not assume an SSH Keychain read failure means login failure.

## Work and acceptance

- [x] Resolve an immutable usable access-token snapshot from an actual native
  process's store only after verifying the selected account and organization
  (Codex: stable subject plus workspace). Reject ambiguous or mismatched sources.
- [x] Keep existing native refresh exclusion. Never copy or refresh a native
  holder's credential, or label a credential renewed when no exchange occurred.
- [x] Make health/list and inference use the same source resolution, including
  exact default-versus-custom Claude identity/Keychain paths.
- [x] Report verified native-managed usable credentials separately from blocked
  renewal. Preserve expired, rejected and unverified states when applicable.
- [x] Replace attempt-only refresh suppression with bounded coordination across
  participating callers, rereading under the lock and guarding late responses.
- [x] On managed failure return an actionable proxy error; preserve explicit
  passthrough and client-bound authentication routes.
- [x] Regression tests: stale slot/current native login, wrong account/org,
  shared workspace/different Codex users, ambiguous stores, inherited child
  environments, native failure, simultaneous refresh and credential replacement.
- [x] Full required Rust tests, clippy and fmt; independent integration review.
- [x] Verify exact macOS slot Keychain coherence for bearer, expiry and renewal
  persistence; a locked/missing authoritative item cannot use a leftover file.
- [x] Recover a genuine HTTP 401 once for the selected account before configured
  failover, without spending a native refresh token or adopting another account
  that replaced the selected credential during the request.
- [ ] Record commit/PR, release channels, installation and service verification
  separately. Keep production account selection and live clients intact.

The expanded competitor survey records 227 discovered candidates and eleven
additional implementation reviews with pinned source references. It does not
claim all GitHub repositories were reviewed or competitor runtime tests passed.

Native resolution is conservative: Claude uses its exact platform store;
Codex requires readable file-backed auth with a stable subject and workspace.
Missing identity, an unreadable store, conflicting usable generations or
unobserved external refresh activity cannot be treated as verified ownership.

Implementation ownership: the native resolver worker owns the bounded 401
request recovery; the coordination worker owns refresh coordination and Mac
credential source coherence; the main agent owns UI, documentation and delivery.
