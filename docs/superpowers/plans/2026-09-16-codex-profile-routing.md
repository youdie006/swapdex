# Codex Profile Routing Implementation Plan

> **For agentic workers:** REQUIRED: Use superpowers:subagent-driven-development. The main agent owns commits, publication, installation, and the stock-client fixture.

**Goal:** Make ordinary Codex profiles use the selected Swapdex serving account.

**Architecture:** Separate profile selection from explicit provider overrides in
the generated POSIX shim. Continue injecting only the built-in OpenAI URL, with
native Codex retaining responsibility for selecting a named custom provider.

**Tech Stack:** Rust, POSIX sh, Python standard library, stock Codex 0.154.0.

## Chunk 1: Regression and minimal fix

**Worker files:** `src/shim.rs`, `tests/proxy.rs` (or an existing focused shim
test module if more appropriate). No other worker edits.

- [x] Run the existing focused shim/routing tests as a baseline.
- [x] Add executable tests for separated and attached profile forms, `login` as
  a profile value, exact argument preservation, and explicit provider bypass.
- [x] Run the tests against the unchanged implementation and record the
  expected missing-proxy-override failure.
- [x] Move `-p|--profile` into the value-consuming managed option branch and
  attached profile forms into a managed no-op branch.
- [x] Run focused tests, `cargo fmt --all -- --check`, and the repository gate.

## Chunk 2: Stock client and release

**Main files:** `scripts/verify-codex-profile-routing.py`, `CHANGELOG.md`, package
versions, this plan, and deployment/release records.

- [x] Build a bounded loopback fixture using fake distinct account tokens and
  a generated installed shim. Verify a `worker` profile sends inference through
  the selected account and preserves the launch home.
- [x] Exercise a profile selecting a custom provider and verify its endpoint
  and credential remain effective.
- [x] Run the fixture against released 0.165.4 to demonstrate the regression,
  then against the candidate to establish the fix without real model requests.
- [ ] Obtain spec and quality reviews; run `scripts/gate.sh` and the installed
  fixture on WSL and M3. Record any unavailable checks honestly.
- [ ] Record the concrete fix in the changelog, commit and push the branch,
  integrate verified work, publish version-specific release notes, and verify
  distribution channels and the installed target versions.
- [x] Re-scan live native process arguments. Report any already-running direct
  jobs separately; do not claim a launcher update reroutes existing processes.
