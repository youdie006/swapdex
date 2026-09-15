# Account Boundary Bugs Implementation Plan

> **For agentic workers:** REQUIRED: Use superpowers:subagent-driven-development (if subagents available) or superpowers:executing-plans to implement this plan. Steps use checkbox (`- [x]`) syntax for tracking.

**Goal:** Keep explicit Claude re-login captures coherent and prevent failed managed proxy launches from using another login.

**Architecture:** Retain explicit Claude slot context through `Paths` and reuse the existing strict slot credential reader. Validate proxy startup before either generated shell launcher executes the native client. The changes have separate file ownership and share sandboxed regression checks.

**Tech Stack:** Rust, existing fs2/serde/test fixtures, POSIX shell, cargo, Python unittest and Node test runner.

## Chunk 1: Reproduce and repair the boundaries

Spec: `docs/superpowers/specs/2026-09-15-account-boundary-bugs.md`.
Baseline: `origin/main` at `72bf802`; `codex_shim` (4) and
`refresh_coordination` (6) tests passed in the isolated worktree.

### Task 1: Claude slot identity and credential capture

**Ownership:** `src/paths.rs`, `src/adapters/claude.rs`, related sections/tests
in `src/commands.rs`, and any dedicated Claude integration test file.

- [x] Extend the existing slot capture fixture with distinct default/slot
  `.claude.json` files. Assert both saved credentials and `oauth_account`
  describe the slot, then run `cargo test --lib slot_capture_tests --locked`
  and record the expected failing identity assertion.
- [x] Add source-selection regression coverage for an unavailable explicit
  slot Keychain with a usable default/file alternative; add a child-login
  environment fixture carrying a conflicting secure-storage override.
- [x] Retain explicit slot context in `Paths` without redirecting other tools
  or changing the store location. Resolve explicit slot identity metadata
  inside the slot and use `slot_credential` for that capture's credential.
  Keep default live-home behavior intact.
- [x] Resolve live identity metadata inside a nonempty explicit
  `CLAUDE_CONFIG_DIR`, including when it equals the default config path.
  Test implicit default, explicit custom/default, and sandbox isolation;
  preserve intentional live secure-storage overrides.
- [x] Clear the secure-storage override only for managed Claude slot login;
  verify the fake child sees the intended config directory and no conflicting
  override in both picker login and `run_account`. Do not change the parent environment.
- [x] Verify a named `run` still reaches the native executable with the shim
  installed and proxy unavailable. Resolve the native tool directly without
  changing the global serving choice, so first login in a new slot works.
  Reject a shim-only PATH with a native-tool diagnostic, and assert existing
  active/serving pointers remain unchanged.
- [x] Keep direct `Paths::rooted` capture, apply and journal recovery from
  consulting the machine Keychain even without a `SWAPDEX_ROOT` environment
  variable. Verify this policy with injected platform availability.
- [x] Run the new regressions and the existing path, capture, provider scoping
  and switch tests. Check missing/corrupt slot metadata and default-home
  behavior. Report exact commands and any platform limitation.

### Task 2: Managed startup must retain its selected route

**Ownership:** `src/shim.rs`, `tests/codex_shim.rs`, a dedicated Claude shim
test file, and startup-state helpers in `src/slots.rs`. Any needed change to
`proxy_ensure` in `src/commands.rs` must be coordinated with Task 1's worker
before editing that shared file.

- [x] Extend the executable shell fixture to control proxy stdout/status and
  observe native-client invocation without assuming success.
- [x] Test startup failure with empty output and with valid-looking output;
  empty successful output; nonnumeric, multiline, zero, negative, too-large,
  and excessively long ports. Run `cargo test --test codex_shim --locked`
  and record failures showing native invocation still occurs.
- [x] Require startup success plus a decimal port in 1..65535. On failure,
  print an actionable diagnostic and exit nonzero before native execution.
  Do not use a new provider ID or alter account pointers.
- [x] Cover the same failure boundary in the Claude generated launcher.
  Preserve actual login and explicit connection choices. Distinguish a known
  unmanaged/explicit passthrough state from failed/invalid selection through
  the Rust startup resolver; test no accounts, passthrough and unreadable or
  malformed selection without silently using display fallbacks.
- [x] Verify valid boundary ports and normal proxy routing, as well as
  authentication, explicit backend/profile/remote and help bypasses even
  while proxy startup is configured to fail.
- [x] Repair the independently confirmed legacy Claude prompt/auth bypass:
  first reproduce `-p login`, `-- login` and a `login` option value executing
  directly. Parse actual authentication commands while respecting print mode,
  option values and the separator; verify real auth remains usable. Treat
  optional debug and variadic MCP config values as ambiguous, and preserve
  supported `auth --help` / `auth -h`.
- [x] Run `cargo test --test codex_shim --test codex_sessions --locked` and
  targeted shim unit tests; report exact commands.

### Task 3: Bound store lock lifetime to its operation

**Ownership:** `src/store.rs` and a dedicated lock regression test if needed.

- [x] Reproduce the reported busy selection with a deterministic inherited
  descriptor test: retain the OS lock descriptor as a forked child would,
  end the owning guard, and attempt the next operation before that descriptor
  closes. It must acquire the lock. Record the expected failure first.
- [x] If reproduced, explicitly unlock in `LockGuard::drop`, preserving all
  active guard exclusion behavior for store, tool and registry locks.
- [x] Verify a retained old descriptor cannot prolong or release a later
  owner's lock, and actual overlapping operations still return Busy.
- [x] Run `cargo test --lib store:: --locked` and the previously failing
  `serve_off_records_unknown_payer_and_serve_name_reenables` regression.

## Chunk 2: Review, record and verify

**Ownership:** main agent owns changelog, these documents, verification records
and all commit/push/PR actions. Workers must not commit or push.

- [x] Review the spec and plan before implementation; resolve findings. The
  independent review approved Tasks 1/2 with explicit metadata layout, both
  managed child entry points, and checked startup state. Main added Task 3
  as a bounded reproduction of the existing suite failure.
- [x] Review each implementation for spec compliance, then code quality.
- [x] Add concrete Unreleased changelog entries for the resulting behavior.
- [x] Run `cargo fmt --all -- --check`.
- [x] Run `cargo clippy --all-targets --locked -- -D warnings`.
- [x] Run `cargo test --all --locked --jobs 2` (1,095 passed, 1 existing ignored).
- [x] Run `python3 -m unittest discover -s .github/scripts -p 'test_deps_automerge.py'` (22 passed).
- [x] Run `node --test 'npm/**/*.test.mjs'` (7 passed).
- [x] Verify normal dependencies exclude banned async/heavy HTTP stacks and
  include rustls/webpki-roots; run `cargo audit`.
### Integration record

The pull request for `fix/account-boundary-bugs` records the pushed commit,
Ubuntu/macOS CI run links and their final results after this source commit.
This document and `docs/research/2026-09-15-account-boundary-verification.md`
record local verification. Release and installation remain separate; existing
processes have not changed.
