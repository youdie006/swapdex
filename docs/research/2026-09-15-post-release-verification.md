# Swapdex 0.164.0 audit verification

Base: `v0.163.0`, commit `855e85c965bcf73907ec5d24033ac23704114836`.
All behavioral reproductions use disposable homes, synthetic logins and local
fake executables/providers. Real selected accounts are not part of the fixture.

## Reproduced boundaries

| Failure | Before | Regression / verification |
| --- | --- | --- |
| Relative native PATH | Generated Claude/Codex shims exit 127 from another cwd | `tests/run.rs`: execute installed shims outside installation cwd; preserve stable native symlinks |
| Non-executable PATH shadow | A regular non-executable file wins discovery; shim exits 126 | `tests/run.rs`: execute the later executable candidate |
| Rooted shell setup | Ambient HOME profile is edited/read instead of supplied root; ambient service pins rooted settings | `tests/run.rs`: distinct root and ambient homes; `Paths::rooted` library coverage |
| Explicit `/swap NAME` | Generated command moves launch-home A to B | `tests/run.rs`: execute generated instructions for both tools; payer B, launch home A |
| systemd path spaces | Unit verifier reports truncated executable as missing | Generated candidate unit with spaces, `%` and `$` passes `systemd-analyze --user verify`; no supervisor loaded |
| launchd XML paths | Raw `&` / `<` are written into XML string content | Generated candidate plist parses with Python `plistlib`; executable/log paths round-trip exactly |
| Codex workspace members | Second distinct JWT subject is already refreshing / skipped after first renewal | `refresh_coordination` plus `codex_refresh_visibility`: two exchanges; malformed identity retains prior grouping |
| Reset-only cache | Future reset times disappear on cache load | `quota_cache`: future resets survive and expire independently |
| Same-name stale Claude profile | Unreadable current slot sends the old profile's token | `quota_identity`: current slot identity and credential only, including a conflicting default-home login |
| Older account conversations | Menu shows only newer other-account sessions | `switch`: 25 same-tool fixtures; older selected-account session appears before limiting results |
| Directory cycles and aliases | One physical transcript is collected 81 times | `native_sessions`: canonical directory tracking, including a symlinked sessions root |
| Session index timeout | Installed 0.163.0 returns after 5.003 seconds while both fake command/descendant survive | Same fixture against candidate 0.164.0 returns after 5.013 seconds with zero survivors; explicit fixture cleanup also verified |

Review added mixed complete/opaque Codex token copies and unrelated live-login
visibility to the regression set. The former initially produced two exchanges
of one rotating token; the latter hid the native row when an unrelated slot
was selected. The quota fixture distinguishes slot usage (12%) from native
usage (31%) and checks both profile-backed and standalone slots.

Final review also reproduced Codex fallback selecting a twin of the refused
account, because candidate deduplication still read Claude identity files.
Complete subject/workspace identities now distinguish members while grouping
actual copies; opaque identities retain conservative workspace grouping.
Another fixture replaced an auth blob after renewal while retaining its old
refresh token and workspace. The reduced health fingerprint let the replacement
inherit `Renewed` without updating it. Coordination now uses full blob
generations, while rejection-health records keep their existing token identity.

## Additional proxy reproductions

Independent loopback fixtures reproduced three boundaries before implementation:

- With configured automatic failover, reselecting rejected Codex A can choose
  a Claude-only slot and return 502 before reaching a Codex upstream. Without
  automatic failover, the same fixture stays on A.
- An old in-flight Claude A request fails after a newer explicit C selection
  has been served. Its late failover writes B; upstream sequence is A, C, B, B.
- A provider fully accepts a POST then resets TCP before its response. The
  Claude path accepts the identical body twice; Codex's nested retries accept
  it 16 times. An orderly close follows a different error classification and
  does not reproduce that retry. These are synthetic submission counts, not
  observations of a real billing statement.

## Verification procedure

1. Run each focused regression against the failing behavior, then its repair.
2. Run `cargo test --all --locked --jobs 2`,
   `cargo clippy --all-targets --locked -- -D warnings`, and
   `cargo fmt --all -- --check` after integrating concurrent work.
3. Run Python dependency-automation tests, all npm tests, dependency-policy
   checks and `cargo audit`.
4. Exercise candidate and installed native executables through the routing and
   stock Codex resume verifiers; publish exact channel/install/runtime results
   in the versioned GitHub release and PR deployment record.

## Final integrated candidate results

- PASS `cargo test --all --locked --jobs 2`: 1,135 passed, zero failed;
  one pre-existing large-streaming test remains ignored. Includes 84 proxy
  integration tests and the cross-process renewal regressions.
- PASS `cargo clippy --all-targets --locked -- -D warnings` and
  `cargo fmt --all -- --check`.
- PASS `python3 -B -m unittest discover -s .github/scripts -p
  'test_deps_automerge.py'`: 22 tests.
- PASS `node --test 'npm/**/*.test.mjs'`: 7 tests.
- PASS dependency policy: no banned async runtime, heavy HTTP stack or
  system-TLS binding; rustls and bundled webpki roots remain present.
- PASS `cargo audit`: no reported vulnerability in the locked dependencies.
- PASS `git diff --check` and Cargo/npm/platform/man-page version agreement.
- PASS `verify-installed-account-routing.py` against the rebuilt candidate:
  generated launchers, named native runs, persistent Claude/Codex A-to-B-to-A
  routing and exactly one accepted POST after a provider-side TCP reset.
- PASS `verify-codex-session-resume.py` with stock Codex and the candidate:
  WebSocket 426 followed by HTTP completion, reproduction and repair of the
  old provider listing/resume failure, and preserved conversation bytes.

Independent loopback repro scripts also pass against the rebuilt candidate:
Codex stays in its own registry, late A failure preserves C, each ambiguous
Claude/Codex POST returns 502 after one accepted body, and a bodyless GET still
retries and succeeds. The proxy regression covers both tools, automatic and
manual modes, and a newer human choice with or without a subsequent request.
Disabling serving during a blocked quota read returns without forwarding or
spinning; repeated selection churn is bounded.

These results verify the source candidate. Publication and WSL/M3 installation
results are recorded separately in the versioned GitHub release and PR.

Systemd syntax was checked against the official
[service command-line documentation](https://github.com/systemd/systemd/blob/main/man/systemd.service.xml)
and [quoting rules](https://github.com/systemd/systemd/blob/main/man/systemd.syntax.xml),
then verified with the machine's actual unit parser. A literal dollar in the
executable token is preserved; doubling it changes the executable name.
