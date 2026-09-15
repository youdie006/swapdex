# Streaming and read-only quota implementation plan

> **For agentic workers:** Use test-driven-development and
> verification-before-completion. Review independently where available.

**Goal:** Deliver available SSE bytes immediately and finish issue #22's
read-only quota requirement.

**Architecture:** Correct the shared response boundary without changing request
selection/replay. Remove renewal from quota orchestration without changing proxy
renewal. These two tasks have separate file ownership.

**Tech stack:** Rust, Hyper HTTP/1 with current-thread Tokio, ureq 3.4.2,
existing tiny_http types, fake-OAuth and loopback fixtures.

## Chunk 1: Streaming

- [x] Reproduce against exact installed 0.165.2 with a gated upstream.
- [x] Add failing Claude/Codex tests in `tests/proxy.rs` for headers, ping and
  completion-event arrival while upstream remains open.
- [x] Implement the response boundary and a listener that can close failed
  streams without completing them; retain the synchronous account handler.
- [x] Verify byte preservation, HEAD/bodyless, HTTP/1.0, 100-continue,
  keep-alive/pipelining, malformed responses and connection shutdown.
- [x] Reproduce the inherited ureq header deadline during body reads and update
  to 3.4.2; verify the body continues beyond the header budget.

## Chunk 2: Quota and delivery

- [x] Compare issue #22's contract with `commands::quota` and existing tests;
  test zero OAuth exchanges and unchanged expired credentials before fixing.
- [x] Update quota orchestration, related tests and documentation.
- [x] Run all locked Rust tests, all-target Clippy with `-D warnings`, fmt,
  Python/npm checks and dependency audit; update `CHANGELOG.md`.
- [x] Review the quota contract and HTTP listener independently; resolve findings.
- [x] Commit and push the feature branch and create PR #37.
- [ ] Integrate after Linux/macOS CI; record delivery in the versioned release.
- [ ] Record exact installed binaries/services after any authorized deployment,
  and publish issue-resolution evidence only for completed work.
