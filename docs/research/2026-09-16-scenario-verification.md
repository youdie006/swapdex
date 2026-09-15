# Account and upgrade scenarios: 0.165.1

## Scope and baseline

This patch continues the first-use audit of 0.165.0, starting from
`2c5c57df4a89bd69b3311b066bd75f3ee288870f`. The user requested additional debugging
across account switching, renewal, concurrent operations and installations.
It is a patch release; these checks do not establish 1.0 readiness.

## Reproduced failures and corrections

| Scenario | Before repair | Verified behavior |
| --- | --- | --- |
| Literal small percentages | `0.5%` stored 50%; `50%%` was accepted | Literal suffix conversion, invalid-input file preservation and fractional display |
| Runtime threshold below 5% | Stored and pinned 0.5% were silently raised to 5% | A ConsumeFirst fixture at 1% moves to its eligible 0.1% account; invalid flags fail before listening |
| Disabled Claude extra usage | An allowed plan with rejected overage became exhausted | Included-plan rejection remains decisive; successful traffic stays available and actual exhausted traffic hands off |
| Quota after token renewal | A successful renewal was followed by a request with the old access token; profile-backed slots skipped renewal | Both layouts use the fresh token on their first usage request |
| Failed/deferred/replaced renewal | Deferral or refusal was swallowed, and quota could use a stale generation | Six fake-OAuth scenarios preserve native ownership and report unavailable reads without sending stale or replacement-account credentials |
| Long settings contention or failed lock open | The command returned success and wrote without a lock | Exit 4 and byte-identical settings; a later unlocked retry succeeds |
| Simultaneous cache writers | Barrier-start writes lost distinct accounts and independent reset/rejection fields | Per-tool transactions preserve updates across threads and child processes |
| Usage reading without reset fields | A later reading erased a future reset learned from traffic | The independent future reset remains, without restamping old measurements |
| Homebrew cleanup after upgrade | Service command named the removed Cellar version | A verified stable opt path launches the new binary after the old directory is removed |
| No automatic alternative | Hysteresis, an unmeasured slot or a disabled slot became evidence of universal refusal/exhaustion | Neutral explanation unless the complete available evidence establishes the stated cause |

The threshold suite passed nine focused tests after the original failures.
Independent review then found two additional diagnostic paths; both real proxy
fixtures failed before repair and passed after it. The service fixture executes
the unchanged installed command after a simulated Cellar cleanup, checks its
child exit status, and rejects decoy, dangling and absent stable links.

The cache reproduction uses eight barrier-start writers over a 12,000-entry
seed, alongside deterministic held-lock thread/process cases. Ten integration
checks and fourteen existing cache unit checks passed after repair. The initial
cache Clippy run found a test-only `useless_vec`; the corrected run passed.

## Intentional behavior retained

Roomiest requires a 10 percentage point headroom improvement. The first small-
threshold fixture incorrectly expected a 0.9-point improvement to trigger that
strategy; it was corrected to ConsumeFirst, with a separate Roomiest control.
The production margin was retained. Unknown usage is never evidence that an
account exceeded a threshold, and lack of a candidate is not proof of refusal.

Failed provider measurements retain their prior reading timestamps. The separate
attempt timestamp continues to space attempts and prevent request stampedes;
waiting after a failed read is intentional pacing, not a fresh measurement.
Successful usage readings intentionally clear previous token-rejection notes.

Store-lock failures leave settings unchanged. A rename/removal already committed
to its registry cannot be rolled back by a later preference failure; that partial
follow-up now emits a warning. The quota cache remains best effort on I/O errors.
Its lock covers local cache work only, with separate locks for each tool and no
nested whole-store lock.

## Verification boundaries

All renewal regressions use fake curl and disposable credentials. They cover
provider refusal, in-use deferral, a running native owner, and credential/account
replacement during an exchange. Existing coordination and late-refusal checks
also passed, including fourteen refresh-coordination tests, sixteen refresh-
health tests and a late in-flight request preserving a newer human choice.

Fixtures do not inspect real credentials, change normal account selections,
contact provider model endpoints or use normal service ports. The Homebrew
upgrade is simulated in a disposable filesystem, not a real brew installation.
The ignored service helper test is explicitly invoked by its parent fixture.

Fresh provider login UI, unseen refresh-token reuse on another machine and an
independent billing ledger are outside these checks. Real installed model probes,
when run, establish the selected request path and provider acceptance, not an
independent billing audit or a promise that providers never expire credentials.

## Candidate checks

- PASS `cargo test --all --locked --jobs 2`: 1,192 passed, zero failed.
  Two entries are ignored: the pre-existing large-streaming test and the service
  fixture helper explicitly exercised by its parent integration tests.
- PASS `cargo clippy --all-targets --locked --jobs 2 -- -D warnings`.
- PASS `cargo fmt --all -- --check` and `git diff --check`.
- PASS `python3 -m unittest discover -s .github/scripts -p test_deps_automerge.py`:
  22 tests, and `node --test 'npm/**/*.test.mjs'`: seven tests.
- PASS locked dependency policy (no prohibited runtime/HTTP dependencies),
  rustls plus bundled roots, and `cargo audit`: 184 dependencies checked.
- PASS six first-use autostart journeys in an isolated Linux network namespace.
- PASS candidate installed-routing verifier: Claude/Codex A-B-A changes,
  accepted POST not replayed and safe bodyless retry.
- PASS stock Codex compatibility verifier: 426-to-HTTP fallback, legacy hidden
  picker reproduction, native listing/resume and preserved conversation bytes.
- Independent review checked thresholds, overage and Homebrew service handling.
  Its two additional diagnostic findings were reproduced and fixed. Follow-up
  independent reviews of settings and quota/cache changes stopped at the agent
  provider's usage limit; the primary agent completed their source review and
  aggregate verification. This is not reported as a completed independent review.

Exact publication, installed native hashes, running service identities and
post-install checks are recorded in the version-specific GitHub release and
its source PR. A mocked renewal test does not certify actual provider login UI.
