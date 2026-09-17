# Quota refresh status implementation plan

> **For agentic workers:** REQUIRED: Use superpowers:subagent-driven-development or superpowers:executing-plans. Only the root agent may commit or push.

**Goal:** An open dashboard explains why a reading is stale and never reports an expired account as ready.

**Architecture:** Preserve failure and observation metadata when cached quota is reused. Reuse read-only credential ownership rules for a deferred-renewal explanation, and make the group summary consistent with account health.

**Tech Stack:** Rust, ratatui, synthetic Unix process and HTTP fixtures, PTY verification.

## Chunk 1: Failure metadata and readiness

- [x] Add failing regression tests for a cached expired/offline/throttled read whose cause and age must both remain visible (for example `login expired · as of 11h`); run the targeted tests and confirm the old behavior fails.
- [x] In `src/commands.rs` preserve failed-read notes during Claude quota cache fallback. In `src/tui.rs` display a reason with the cached observation age.
- [x] Add and run failing fleet tests for stale, warning and paused members; keep existing spent/credits tests meaningful.
- [x] Correct `fleet_of` in `src/tui.rs` so unavailable credentials are excluded from ready count and available quota aggregation and reset forecast; retain valid spent-account/reset behavior.
- [x] If necessary, add a read-only Claude renewal-deferred predicate in `src/refresh.rs`, and use it in `quota` to explain an expired slot with a live credential holder. Extend `tests/quota_renewal.rs` to distinguish ordinary expiry from an expired held login and verify both without OAuth or credential writes.
- [x] Run `cargo test --all --locked`, `cargo clippy --all-targets --locked -- -D warnings`, and `cargo fmt --all -- --check`.
- [x] Verify old cached numbers, failure cause plus age, and later native-login recovery in a real PTY with synthetic credentials; no real model calls or account changes.
- [ ] Review requirements and then code quality, record concrete changelog and validation, commit/push and finish the PR within the existing bugfix authorization.

## Delivery

Treat source, release, installation, and running process state separately. Do
not claim the user's expired login has recovered based on display-only changes.
