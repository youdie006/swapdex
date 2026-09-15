# First-use audit implementation plan

> **For agentic workers:** Use superpowers:subagent-driven-development for
> independent tasks. Only the main agent commits or pushes.

**Goal:** Make the first installation, account setup and managed launch work
with the tool the user actually has, with accurate guidance and failure states.

**Architecture:** Retain the existing commands and account model. Separate
optional shim installation, enforce installer success conditions, and verify
the documented sequence through real binaries with isolated fixtures.

**Tech Stack:** Rust CLI, POSIX shell installer, Python fixture orchestration.

## Chunk 1: First-use command behavior

Files: `src/commands.rs`, `src/shim.rs` if needed, `tests/run.rs` or a dedicated
`tests/first_use.rs` integration suite.

- [x] Reproduce Codex-only shim and onboarding failures on installed 0.164.1.
- [x] Add regressions for Codex-only, Claude-only, both tools, neither tool,
  tool-specific PATH precedence and relevant missing-shim onboarding prompts.
- [x] Run the regressions before changing behavior; preserve expected failures.
- [x] Make each available tool independently installable and print accurate
  tool-specific follow-up commands. Qualify setup's next-message promise.
- [x] Run focused tests, all-target Clippy and formatting after the repair.

## Chunk 2: Installer failure behavior

Files: `install.sh`, a dedicated installer test suite; wire it into CI if the
chosen test runner is not already included.

- [x] Independently reproduce empty/missing checksum, missing hasher and
  unexecutable candidate behavior without touching a real installation.
- [x] Add failing acceptance cases that preserve a prior working installation.
- [x] Fix confirmed causes and run the POSIX shell on supported success paths
  and failure paths. Verify a valid candidate before replacing an installed one.

## Chunk 3: Documentation and integration

Files: `README.md`, `docs/COMMANDS.md`, `src/banner.rs` only if an actual entry
point needs a corrected hint, `CHANGELOG.md`, version metadata for release.

- [x] Exercise the documented first-use path from a fresh synthetic home and
  an already signed-in synthetic home. Check first request, switch and resume.
- [x] Correct the quickstart and obsolete network/session behavior claims;
  preserve the legacy snapshot workflow in its own section.
- [x] Review the independent changes and run `cargo test --all --locked`,
  `cargo clippy --all-targets --locked -- -D warnings`, formatting, Python/npm
  tests, dependency policy/audit and installed routing/resume checks.
- [ ] Record concrete changes, commit/push, obtain Linux/macOS CI and publish
  a new version. Verify each channel before WSL/M3 installation.
- [ ] Verify installed native hashes, service PIDs/builds, preserved account
  selection, installed behavior and a bounded real request per target; preserve
  the final release/PR deployment record.
