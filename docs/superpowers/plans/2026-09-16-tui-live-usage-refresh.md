# TUI Usage Refresh Implementation Plan

> **For agentic workers:** use the available subagent-driven-development workflow
> for the bounded implementation below. Only the main agent commits or pushes.

**Goal:** Usage and Quota panels update while open and remain responsive.

**Architecture:** A panel refresh controller owns the in-flight read, last
completion time and refresh request. TuiCtx starts asynchronous reads;
production owns a child process with temporary output files and cancels/reaps
its process group when a panel is abandoned. Tests use controlled channels.

**Tech Stack:** Rust, ratatui/crossterm, std::sync::mpsc; no new dependencies.

## Task 1: Regression and implementation

Files: `src/tui.rs`, `src/commands.rs`; tests colocated or a focused integration
test if existing test infrastructure supports the real loop.

- [x] Read applicable skills, AGENTS.md and CONTRIBUTING.md before edits/builds.
- [x] Run baseline `cargo test --all tui::` in this worktree.
- [x] Add focused failing tests for the behavior in the design document.
- [x] Run the tests and record the expected regression failures.
- [x] Implement the smallest refresh controller and asynchronous production reads.
- [x] Preserve existing account identity-generation guards and avoid changes to
  authentication, proxy routing, Doctor behavior or unrelated UI.
- [x] Run focused tests, then `cargo test --all`,
  `cargo clippy --all-targets -- -D warnings`, and
  `cargo fmt --all -- --check`.
- [x] Report exact verification commands and file/line citations; no commits.

## Task 2: Review, records and deployment (main agent)

- [x] Review the diff and exercise input/render behavior in a fake home.
- [x] Add the concrete fix to CHANGELOG.md and durable verification notes.
- [x] Resolve relevant review findings, rerunning checks affected by changes.
- [ ] Commit/push verified work, publish version-specific release notes if
  released, and distinguish source, publication and installation states.
- [ ] Install on authorized target(s), verify the executable version and active
  service identities, and record the evidence in the release or PR.
