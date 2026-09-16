# Codex profile account routing verification

## Problem and resulting behavior

An ordinary `codex --strict-config exec -p worker ...` launch passed through
the installed Swapdex shim, but the shim classified every profile option as an
explicit backend choice. It omitted the proxy URL, so stock Codex used the login
in its launch home even when a different serving account was selected.

Version 0.165.5 keeps all four `-p`/`--profile` forms managed. It preserves the
original arguments, profile, credentials, and session home while routing the
built-in OpenAI provider through the selected serving account. A named custom
provider retains its URL and API key; explicit command-line provider overrides,
remote/local providers, and authentication commands retain their existing
passthrough behavior.

The launcher change applies to new native processes. It does not cancel,
replay, or change an already-running direct job's connection.

## Reproduction and behavioral checks

The observed background jobs had a profile option and no proxy override. Their
launch account and the proxy's serving account were different. The jobs ended
naturally during the investigation. Usage was read from the provider's live
quota endpoint, rather than inferred from the local cache. A quota percentage
alone does not attribute an individual request or establish when billing stops.

`scripts/verify-codex-profile-routing.py` exercises stock Codex 0.154.0, an
installed-shim-shaped launcher, fake distinct accounts, and loopback endpoints.
It creates isolated configuration and session homes and reaps its own processes.
It sends no real model requests and does not access running user sessions.

| Check | Result |
| --- | --- |
| Released 0.165.4 with `-p worker` | Expected failure: native Codex reached the rejecting direct endpoint. |
| Candidate 0.165.5: `-p worker`, `-pworker`, `--profile worker`, `--profile=worker` | All four completed through the selected account, with the launch credential unchanged. |
| Candidate 0.165.5: named custom-provider profile | Completed through the configured custom endpoint and API key, without using the managed account. |
| Executable Rust shim regressions | Exact arguments/home, profile value `login`, explicit provider overrides, and auth commands covered. |
| Installed-routing fixture against candidate | Passed. |
| First-use fixture against candidate | Six foreground cases passed; this mode does not verify autostart. |

Codex 0.154.0 uses `<name>.config.toml` for profile selection. The first fixture
attempt used the legacy `[profiles.worker]` form and failed strict configuration
validation; that setup failure was not counted as the regression reproduction.
The corrected fixture failed against 0.165.4 and passed against 0.165.5.

## Local verification

- `cargo test --locked --all --jobs 2`: 1,222 passed, 0 failed, 2 intentionally
  ignored. The complete suite passed after the final fixture mutex change.
- `cargo clippy --locked --all-targets --jobs 2 -- -D warnings`,
  `cargo fmt --all -- --check`, and `sh scripts/gate.sh`: passed.
- `python3 -B -m unittest discover -s .github/scripts -p 'test_deps_automerge.py'`:
  22 passed.
- `node --test 'npm/**/*.test.mjs'`: 7 passed.
- `ruff check scripts/verify-codex-profile-routing.py`, Python parsing,
  normalized generated man-page comparison, and `git diff --check`: passed.
- Independent spec and code-quality reviews approved the functional change.
  The stale packaged man-page version found in review was corrected. Fixture
  temporary directories now clean up on assertion failure as well as success.

The initial Rust regression setup exposed a duplicate temporary-directory nonce;
the corrected pre-fix run failed because the proxy override was absent. Updating
the older bypass expectation was also necessary. A later parallel fixture run
encountered Linux `ETXTBSY`; its immediate repeat passed. The fixture write/fork
lifetime is serialized in the final change, consistent with the existing
executable shim suite, without retrying or suppressing assertion failures.
Twenty consecutive runs with seven parallel test threads passed all 140
focused cases after the mutex change; the full suite and gate then passed.

## Installation and publication record

A parser-only interim fix was applied atomically to the existing managed Codex
shims on WSL at 2026-09-16 00:51 UTC and M3 at 00:52 UTC. Shell syntax and exact
argument-preservation checks passed. Account-selection pointers were unchanged;
no proxy service or native session was restarted for that interim fix. The
installed native packages remained 0.165.4 at that point.

The version-specific GitHub release and PR delivery records are the durable
source for the final tag/commit, distribution checks, installed 0.165.5 targets,
and running service verification. Package publication alone is not evidence
that an installed executable or running process has been updated.
