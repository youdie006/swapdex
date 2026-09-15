# Account boundary regression verification

Date: 2026-09-15. Base: `72bf8020ffb9f82825ced5e5cc709911f49d08a6`
(`v0.162.0`). Branch: `fix/account-boundary-bugs`.

This record covers source changes and synthetic regression tests. Release
publication, installation and changes to existing accounts or services are
separate actions. The pull request records the final commit and CI runs.

## Reproductions and resulting behavior

| Boundary | Reproduced failure | Resulting behavior |
| --- | --- | --- |
| Claude re-login capture | A slot credential was saved with the default home's account UUID. Explicit config layouts also read the wrong identity file. | Credential and identity follow the same selected config. Missing or corrupt slot metadata cannot borrow the default identity. |
| Claude credential selection | An unavailable explicit slot could fall back to a live credential; inherited secure-storage configuration redirected managed children. | Strict capture uses only its slot source. Managed Claude run/sign-in children clear the conflicting override; ordinary live callers retain intentional configuration. |
| Managed client startup | Both generated launchers executed the native client after failed startup or invalid port output. Broken account pointers could appear unmanaged. | Successful managed startup requires a decimal port in `1..65535`. Only checked unmanaged/off state permits direct launch; invalid state stops before native execution. An existing proxy remains usable across off/on selection changes. |
| Claude command parsing | `-p login`, `-- login`, optional debug values and variadic MCP config values could bypass the selected payer. Actual `auth status` and nested auth help could incorrectly require startup. | Prompt/option values retain managed routing. Supported auth commands and help run directly. Ambiguous option forms cannot authorize an auth bypass. |
| Named account launch | An installed shim intercepted `run` before a fresh account could sign in. A missing native executable could re-enter that shim. | Named launches resolve the native executable and leave existing active/serving pointers unchanged. A missing native tool reports an error in both named run and picker sign-in. |
| Rooted Claude operations | Direct library callers could bypass the environment-only sandbox check during capture, apply or journal recovery. | Rooted paths disable machine Keychain operations even when the process has no `SWAPDEX_ROOT` variable. |
| Operation lock lifetime | Retaining a cloned lock descriptor kept the next operation busy after its owning guard ended. | Store, tool and registry guards explicitly unlock on drop; a later owner remains protected from both concurrent attempts and closure of the old descriptor. |

The lock regression deterministically reproduces the descriptor lifetime
fault. It does not establish that every historical busy error had this cause.

## Verification

Focused tests first failed on the behavior under repair, then passed after
the corresponding changes. These included:

- `cargo test --lib slot_capture_tests --locked`
- `cargo test --lib capture_credential_selection_tests --locked`
- `cargo test --test claude_capture --locked`
- `cargo test --test run managed_claude_run_clears_a_conflicting_secure_storage_override --locked`
- `cargo test --test run run_steps_over_installed_shims_for_fresh_accounts --locked`
- `cargo test --test run run_reports_when_an_installed_shim_has_no_native_tool_behind_it --locked`
- `cargo test --lib rooted_paths_disable_machine_keychain_operations --locked`
- `cargo test --test codex_shim --locked`
- `cargo test --test claude_shim --locked`
- `cargo test --lib store::tests::lock_guard_release_is_not_extended_by_a_cloned_descriptor --locked -- --exact`

Final integrated verification passed after the review follow-ups. Independent
review covered capture/native launch, proxy startup/command parsing, and lock
ownership; the main reviewer also checked the final help/option boundary. All shell tests execute the generated script with fake clients;
they do not call a model or authenticate an account.

| Check | Result |
| --- | --- |
| `cargo fmt --all -- --check` | PASS |
| `git diff --check` | PASS |
| `cargo clippy --all-targets --locked --jobs 2 -- -D warnings` | PASS |
| `cargo test --all --locked --jobs 2` | PASS: 1,095 passed, 0 failed, 1 existing large-streaming test ignored |
| `python3 -m unittest discover -s .github/scripts -p 'test_deps_automerge.py'` | PASS: 22 tests |
| `node --test 'npm/**/*.test.mjs'` | PASS: 7 tests |
| `cargo tree -e normal --locked` dependency policy | PASS: banned async/heavy HTTP stacks absent; rustls and webpki-roots present |
| `cargo audit` | PASS: no advisories reported |

## Platform and deployment limits

Local verification runs on Linux/WSL. Four CLI environment-layout tests are
Linux-only; pure layout/source-selection tests and rooted capture fixtures
also run on macOS CI. These fixtures use synthetic credentials and do not
verify a real user's Keychain permissions or repair an existing saved account.

Claude argument shapes were checked against installed Claude Code `2.1.272`
help and its supported `auth --help` / `auth -h` commands. Optional debug and
variadic MCP configuration arguments are deliberately treated as ambiguous
by the launcher. Fixed plugin-path and plugin-URL arguments consume one value.

GitHub CI runs the repository checks on Ubuntu and macOS 14. Its final status
and run links belong to the pull request for the pushed commit. The initial
fix commit was source-only. The user subsequently requested release and
installation; the follow-up below records preparation, and PR #30 plus the
version-specific GitHub release record publication and machine verification.


## 0.163.0 release preparation

The release candidate carries matching Cargo/lock/npm/platform-pin/man-page
versions and moves these fixes into the 0.163.0 changelog section. The optimized
Linux build, full Rust suite (1,095 passed, one existing ignored), Clippy, format,
Python (22) and Node (7) checks passed again.

A pre-release rerun exposed a test-fixture concurrency fault: another test's
fork could retain a newly written executable's writable descriptor, causing
`exec` to return `ETXTBSY`. The unpatched Claude stress run failed on iteration
18. Serializing fixture construction and process launch fixed the test harness;
Claude passed 300 consecutive runs and Codex passed 100. Production launcher
logic was unchanged by this follow-up.

The new `scripts/verify-installed-account-routing.py` utility runs against an
absolute installed native executable without a Rust toolchain. Main review
reproduced the managed-startup failure against installed 0.162.0, then verified
the optimized 0.163.0 candidate: generated shims, direct named runs, preserved
selection pointers, and coherent Claude/Codex A-to-B-to-A payer changes through
one persistent proxy and client connection. These are synthetic account tests;
they do not consume a real account's quota or modify the target's account store.

Protected-branch integration, publication and target installation results are
recorded on [PR #30](https://github.com/youdie006/swapdex/pull/30) and the
[0.163.0 release](https://github.com/youdie006/swapdex/releases/tag/v0.163.0)
once those operations complete. Local verification alone does not establish
that a running service has been updated.
