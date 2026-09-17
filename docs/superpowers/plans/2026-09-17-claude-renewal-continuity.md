# Claude Renewal Continuity Implementation Plan

> **For agentic workers:** Use the existing development and verification skills. Reviewers work read-only; only the main agent commits or publishes.

**Goal:** Renew managed Claude authentication while existing conversations remain open, without reusing stale copied refresh tokens.

**Architecture:** Share the native refresh locks and pin the exact credential authority. Keep identity verification, generation checks, and durable writes inside the coordinated operation. Preserve independent logins and native configuration.

**Tech Stack:** Rust, existing fs2 and serde support, Python isolated stock-client exercises.

## Chunk 1: Native refresh synchronization

- [x] Add a focused native directory-lock module and tests for exclusion, acquisition order, release, stale recovery, and replacement protection.
- [x] Verify its protocol against stock Claude 2.1.271–2.1.274 using synthetic credentials and blocked external network.
- [x] Connect it to Claude renewal, rereading credentials after acquisition and preserving all post-response generation checks.

## Chunk 2: Credential authority

- [x] Prove exact storage-root and Keychain rules for default versus explicit native configuration.
- [x] Add authority descriptor validation and tests for same-generation association, identity mismatch, independent logins, and process exit.
- [x] Route access reads, renewal writes, and native launches through the pinned authority without moving session history or MCP configuration.
- [x] Test re-login/logout behavior and prohibit stale fallback when authority cannot be verified.

## Chunk 3: Delivery

- [x] Exercise two renewals and successful next turns in one stock native process; verify authority after exit and generated relaunch independently.
- [x] Run `cargo test --all --locked`, `cargo clippy --all-targets -- -D warnings`, `cargo fmt --all -- --check`, and relevant client scripts against the candidate.
- [ ] Update the changelog with concrete behavior; complete read-only review and commit/push the verified source.
- [ ] Publish version-specific artifacts and verify each installation channel.
- [ ] Install on WSL and M3, verify running processes and versions, and preserve results in release records.
