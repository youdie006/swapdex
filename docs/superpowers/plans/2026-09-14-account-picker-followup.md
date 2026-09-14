# Account picker follow-up implementation plan

> **For agentic workers:** Use superpowers:subagent-driven-development. The user has authorized continued repairs, Git publication, and WSL installation.

**Goal:** Keep picker actions on the selected provider and explain when an open picker needs restarting after an installation update.

**Architecture:** Build one picker row per provider and carry that provider into account actions instead of inferring it again from a shared name. Keep the existing aggregated CLI listing compatible. Internal picker subprocesses must reject an unavailable executable with a clear restart instruction; they must not revive the deleted binary or choose a different executable from PATH.

**Tech Stack:** Rust, ratatui/crossterm, isolated filesystem fixtures and PTY integration tests.

## Provider-specific rows and actions

Files: `src/commands.rs`, `src/tui.rs`, `src/session_link.rs`, `src/native_sessions.rs`, focused integration tests.

- [x] Reproduce a Codex profile sharing a name with a Claude slot: inspect provider, login state and selection target.
- [x] Build provider-specific rows and resolve slot, identity, active status and health within that provider.
- [x] Carry the selected provider into switching, sign-in, deletion and conversation selection.
- [x] Keep quota lookup from applying the other provider's reading to a same-named row.
- [x] Filter recent conversations by provider before applying limits, including sessionwiki and native fallbacks.
- [x] Ignore stale snapshot identity and warnings when the row represents a live slot.
- [x] Verify same-name accounts and existing single-provider behavior using fake credentials and `SWAPDEX_ROOT`.

## Executable replacement during an open picker

Files: `src/self_exe.rs`, `src/lib.rs`, `src/commands.rs`, `tests/picker_upgrade.rs`.

- [x] Reproduce the exact ENOENT by starting a copied binary in a PTY, unlinking the copy, then selecting a synthetic slot.
- [x] Require an actionable restart message and unchanged serving pointer in the regression test.
- [x] Add a small guarded self-command helper and use it for picker subprocesses.
- [x] Verify a normal installed binary can still select an account.

## Review and release

Files: `Cargo.toml`, `Cargo.lock`, `npm/package.json`, `CHANGELOG.md`, generated npm release metadata.

- [x] Review integrated diff for provider leakage and unintended live side effects.
- [x] Run `cargo fmt --all -- --check`, `cargo clippy --all-targets -- -D warnings`, `cargo test --all`.
- [x] Run `python3 -m unittest discover -s .github/scripts -p 'test_deps_automerge.py'` and `node --test 'npm/**/*.test.mjs'` (22 and 7 passed).
- [ ] Commit and push fixes; wait for CI before publishing the release tag and packages.
- [ ] Install the published WSL package and apply the running proxy update while preserving account choices.
- [ ] Check installed version, synthetic picker behavior and the existing Codex `work` preference.

All development commands use temporary stores and fake credentials. Real token refresh, login, probes and copying credential material are outside this repair. Empty local Claude registration cleanup was completed separately without removing its directory or changing Codex records.
