# Claude login continuity after the 0.165.7 recovery

## Required outcome

The user requires automatic renewal that avoids repeated browser logins while
existing Claude conversations remain open. Restoring today's credential and
explaining a deferred renewal do not satisfy that requirement.

## Verified gaps at the 0.165.7 baseline

- `refresh_slot_inner` returns `NativeManaged` when a matching running Claude
  has usable access, and otherwise returns `InUse` while that account has a
  native holder. The keep-alive timer therefore does not establish that an
  idle, long-running native process will renew before its refresh token dies.
- The live-login resolver borrows access only while a native process exists.
  It does not establish a persistent credential authority after that process
  exits. A separate slot can retain a previous refresh generation.
- The earlier local recovery restored the complete newly authenticated OAuth
  block into the default native store. This restored the existing sessions'
  login, but also left the default store and the managed slot holding the same
  refresh generation. That repair is not an ownership protocol.
- The exact historical exchange that caused the original empty native OAuth
  fields has not been established. A narrowly filtered native debug-log check
  found no `invalid_grant` or dead-refresh-token clearing events to attribute.

## Baseline checks

At source `8d265e7`, `cargo test --locked --test refresh_coordination --
--test-threads=1` passed all 14 tests. Some of those tests deliberately assert
that an expired native holder blocks renewal. Passing this baseline does not
prove continuous renewal with an open native session.

An isolated stock Claude 2.1.274 check used synthetic credentials with
`expiresAt: 1`, a nonempty synthetic refresh token, and blocked provider
network access. `auth status --json` returned exit 0 and `loggedIn: true` in
0.25 seconds, leaving the expired credential unchanged. Auth status is
therefore not an OAuth renewal entrypoint or evidence that an access token
has a usable deadline.

## Acceptance criteria for the repair

- One designated store owns each managed refresh-token chain. Account and
  organization identity alone must not merge independently issued chains.
- Supported native and Swapdex refreshers must share exclusion across the
  credential reread, OAuth exchange, and durable write. Merely removing the
  existing holder guard is insufficient.
- A matching open native conversation observes the renewed credential on a
  subsequent turn without termination or a browser login.
- Closing the native process cannot reactivate an obsolete copied refresh
  token. A newly started conversation uses the same designated authority.
- Explicit login changes, unrelated account identities, independent token
  chains, locked Keychains, uncertain ownership, and failed writes remain
  distinguishable and must not overwrite another login.
- Prove two successive renewals using synthetic tokens and a loopback OAuth
  service, including concurrency, process exit, and credential replacement.
- Validate source, published artifacts, installed versions, and running
  services separately. Record limitations rather than promising recovery of
  a provider-revoked credential or renewal while the machine is offline.

## Implemented repair

- Claude 2.1.271–2.1.274 holders share the native two-directory refresh locks.
  The holder keeps a five-second heartbeat, validates ownership before exchange
  and persistence, and recovers unchanged empty locks after the native
  sixty-second stale interval. Native and Swapdex both have a path-based
  stale-check/removal boundary; this is not an atomic compare-and-remove API.
- A slot may bind to a supported native source only with matching account,
  organization, and exact refresh-token generation. The private descriptor
  pins the storage root, identity path, and bare/hashed Keychain selection.
  The selected source remains authoritative after its native process exits.
- Proxy serving, quota reads, strict slot capture, renewal, and managed
  launches follow that descriptor. Ordinary launches keep their session config;
  explicit native authentication updates the designated login source. Logout
  leaves inference unavailable but permits signing the same account in again.
  Leading native options, including `--verbose auth login`, preserve that route.
- HTTP 401 recovery can renew the exact authoritative blob once and retry the
  same account. Unproven sources, changed identities, and independent logins
  never authorize spending a copied refresh token.
- Missing slot identity no longer hides a native holder of the same refresh
  generation. Unverified native releases retain the deferral guard.
- `endpoint busy` was the dashboard's wording for HTTP 429 from the usage API.
  It now says `usage lookup limited - retrying`; prior numbers retain their
  observation time. This response says nothing about login validity or the
  account's model allowance.

## Verification evidence

The development branch is `fix/claude-renewal-continuity`, based on
`8d265e7dd63256bfc95e564b6bea94e00d693d90`. No real provider model request or
real OAuth exchange was needed for the implementation checks.

| Check | Result |
| --- | --- |
| Native directory locking | 12 focused tests, including stale locks, fresh heartbeat, successor identity, nonempty directories and future timestamps |
| Durable authority | 9 focused tests, including logout/re-login, separate generations, identity changes, process exit, write source and launch environment |
| Proxy regression | A linked bearer initially returned HTTP 401 without renewal; after the fix, native-alive and native-exited cases each exchange once and return success |
| Missing-identity holder | Before the fix it spent the native holder's generation; after the fix it returns `InUse` with zero OAuth calls |
| Native version gate | Unknown native sources cannot create authority; verified standalone and npm native installations are recognized |
| Stock WSL Claude | 2.1.271, 2.1.272, 2.1.273, 2.1.274 each survived two consecutive synthetic renewals in one process |
| Stock M3 Claude | 2.1.274 survived two consecutive synthetic file-store renewals in one process |
| Generated launcher | Isolated executable test preserves session home, auth source and literal argv; logged-out inference and a dangling authority marker fail closed |
| General behavior | First-use foreground scenarios, installed-launcher A-B-A routing and streaming checks passed with synthetic accounts and loopback providers |
| Review | Read-only review found and drove fixes for crash recovery, missing-identity holders, unsupported binding and authentication after logout/leading options |

Release-candidate source checks at version 0.165.8:

- `cargo test --all --locked -q`: **1,268 passed, 0 failed, 2 ignored** across
  30 test targets, including documentation tests.
- `cargo clippy --all-targets --locked -- -D warnings`: passed.
- `cargo fmt --all -- --check` and `git diff --check`: passed. Generated man-page
  trailing whitespace initially failed the diff check and was removed before
  the successful rerun.
- `node --test 'npm/**/*.test.mjs'`: 7 passed.
- `python3 -m unittest discover -s .github/scripts -p 'test_deps_automerge.py'`:
  22 passed.
- `scripts/verify-first-use.py --proxy-mode foreground`,
  `scripts/verify-installed-account-routing.py`, and
  `scripts/verify-streaming.py`: passed against the built candidate.

The stock-client fixture uses a fake curl OAuth exchange, loopback SSE model
responses and blocked external HTTPS. Every successful run records the same
native process, refresh generations 1 and 2 each spent once, and model requests
using access generations 2 and 3. It checks that inference does not start while
the refresh locks are held and cleans up its children.

The default first-use autostart check could not run on WSL because the real
8787 service occupied its fixed port. The explicit `--proxy-mode foreground`
variant passed without stopping that service; CI remains responsible for the
unoccupied-port autostart case. Native macOS Keychain creation under SSH timed
out before an OAuth/model turn; attribute-only cleanup checks found the unique
fixture items absent. File-store native interoperability and pure Keychain
source-selection tests do not establish a successful live Keychain exchange.

Known different-account replacements of the shared native store remain an
explicit authority error. They cannot reactivate the obsolete slot copy.
No normal remove/adopt/capture operation deletes the descriptor; manual
descriptor removal is outside the authority continuity guarantee.

## Release and installation record

The candidate release is **0.165.8**. Publication, artifact hashes, registry
checks, WSL/M3 installation versions, service verification, and any unavailable
checks are recorded in the version-specific GitHub release and PR after their
corresponding steps finish. At this source-record stage, installation remains
pending; a successful repository check does not update a running process.
