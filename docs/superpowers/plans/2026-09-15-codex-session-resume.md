# Codex Session Resume Implementation Plan

> **For agentic workers:** REQUIRED: Use superpowers:subagent-driven-development (if subagents available) or superpowers:executing-plans to implement this plan. Steps use checkbox (`- [x]`) syntax for tracking.

**Goal:** Restore normal Codex discovery and resume for existing conversations and prevent account switching from hiding future sessions.

**Architecture:** Keep the stock native local TUI and stable `openai` provider, changing only its proxy endpoint. Repair legacy provider metadata through a private, recoverable fixed-width journal and conditional SQLite updates. Preserve the complete conversation and paginated history stores.

**Tech Stack:** Rust, existing synchronous tiny_http/ureq proxy, serde_json raw values, bundled SQLite, POSIX launch shim, stock Codex 0.154.0 runtime verification.

---

## Chunk 1: Transport and history compatibility

### Task 1: Codex HTTP fallback

**Files:** `src/proxy/mod.rs`, new `tests/codex_websocket.rs`.

- [x] Add an isolated proxy test sending a Responses WebSocket upgrade with no usable account; expect 426 and no upstream request.
- [x] Run `cargo test --test codex_websocket --jobs 1` and record the expected failure.
- [x] Return 426 for Codex Responses WebSocket upgrades before `forward_turn`; retain all ordinary request behavior.
- [x] Run the focused test and existing proxy tests. Update printed manual configuration to use `openai_base_url`.

### Task 2: Recoverable legacy provider repair

**Files:** new `src/codex_sessions.rs`, new `tests/codex_sessions.rs`, `src/lib.rs`, `src/main.rs`, `Cargo.toml`, `Cargo.lock`.

- [x] Define `repair-codex-sessions [--dry-run] [--quiet]` and a library result with repaired/skipped/error counts.
- [x] Add failing isolated tests for legacy and unrelated providers, existing per-slot indexes, archived roots, dry-run and idempotency.
- [x] Implement bounded first-line parsing, provider token span discovery, private flushed journal, fixed-width replacement and conditional SQLite updates.
- [x] Add interrupted-write recovery and concurrent append-handle tests; check that every byte after the original header and every history item remains identical.
- [x] Acquire native per-thread writer locks across known shared homes, defer busy threads, and test explicit warnings for unsupported compressed rollouts.
- [x] Run `cargo test --test codex_sessions --jobs 1`, `cargo test --all --jobs 1`, `cargo clippy --all-targets -- -D warnings`, and `cargo fmt --all -- --check`.

### Task 3: Stable routing and automatic repair

**Files:** `src/shim.rs`, existing `tests/proxy.rs`, `tests/run.rs`, new shim regression tests if useful.

- [x] Replace obsolete provider-name assertions with behavioral tests: ordinary/resume/fork/exec launches receive `openai_base_url`; account changes never set `model_provider=swapdex*`.
- [x] Add tests proving login/logout bypass and literal prompt words/option values do not select the login branch; preserve explicit profile/provider/remote choices.
- [x] Run the tests and record the expected failure.
- [x] Resolve `CODEX_HOME` before invoking repair; invoke repair before history-capable launches. Show repair failure on stderr without pretending it succeeded.
- [x] Configure the proxy through `-c openai_base_url=http://127.0.0.1:$port/v1`; preserve original argv and user override precedence. Payer identity stays in Swapdex's existing status surfaces.
- [x] Run focused shim/proxy/run checks, then the required repository checks.

## Chunk 2: Native verification and release

### Task 4: Verify the real client

**Files:** a bounded isolated native-Codex verification script under `scripts/`, release verification record under `docs/`.

- [x] Reproduce the missing-provider failure with a synthetic legacy session and stock Codex; repair it and verify native picker/direct resume without provider overrides.
- [x] Verify built-in OpenAI routing against a local fake-OAuth Responses fixture; assert one 426 probe, successful HTTP response and no external model request.
- [x] Apply repair on the affected machine only after journal and preservation checks pass; verify the original gitstar and deck sessions at idle and compare paginated item hashes.
- [x] Independently review implementation for spec compliance and data integrity, and resolve material findings.

### Task 5: Publish and install

**Files:** `CHANGELOG.md`, Cargo/npm version files, `npm/man/swapdex.1`, durable release record.

- [x] Choose the next unused version; record the actual failure and resulting behavior in the changelog and release notes.
- [x] Run Cargo test/clippy/fmt, npm tests, Python CI tests, dependency policy and audit; record all results honestly.
- [ ] Commit, push, open and merge a concrete PR after checks. Tag the verified commit and verify all four release assets.
- [ ] Publish and verify npm, crates.io and Homebrew according to CONTRIBUTING.md.
- [ ] Install the exact npm version on WSL, regenerate shims, restart existing proxies without changing selected accounts, and verify executable/version markers and native resume.
- [ ] Append commit/tag, channel and installation verification to durable release/PR records; report the installed result and links.
