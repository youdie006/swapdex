# Proxy SIGPIPE implementation plan

> **For agentic workers:** REQUIRED: Use superpowers:subagent-driven-development
> (if subagents available) or superpowers:executing-plans to implement this plan.
> Only the main agent may commit, push, publish or change real services.

**Goal:** Keep a disconnected client from terminating the shared proxy.

**Architecture:** Ignore SIGPIPE at proxy entry before listeners or workers are
started. Let existing I/O error handling close the failed connection. Preserve
the CLI's default SIGPIPE policy outside the proxy.

**Tech stack:** Rust, libc, Hyper/Tokio, isolated Rust/Python loopback fixtures.

## Chunk 1: Regression and minimal fix

Files: `src/proxy/mod.rs`, `tests/proxy.rs` or a focused new integration test,
and `scripts/verify-streaming.py` if extending the installed fixture is useful.

- [x] Read relevant skills and repository instructions before editing.
- [x] Start the real executable in a temporary `SWAPDEX_ROOT`; use fake accounts
  and a loopback provider only. Bound every wait and reap every child.
- [x] Add a regression for SIGPIPE in a serving proxy and confirm failure on
  the unmodified executable. Test both tools and subsequent same-process service.
- [x] Exercise client disconnection without injecting a real model request.
- [x] At proxy entry, install `libc::SIG_IGN` for `libc::SIGPIPE` before threads.
  Explain why `main` has a different policy. Make no unrelated changes.
- [x] Verify regression green and `cargo test --locked --test pipe` green.
- [x] Run `cargo test --locked --all --jobs 2`,
  `cargo clippy --locked --all-targets --jobs 2 -- -D warnings`, and
  `cargo fmt --all -- --check`; report exact results and review the diff.
- [x] Obtain spec compliance and code-quality review before integration.

## Chunk 2: Release and installed verification (main agent)

Files: release versions/man page, `CHANGELOG.md`, and release/PR delivery records.

- [x] Record the concrete disconnect/restart fix and align version 0.165.4
  across Cargo, lockfile, npm and generated man page.
- [ ] Verify metadata, npm tests and installed routing/streaming fixtures.
- [ ] Commit and push the reviewed change; wait for Linux/macOS CI before merge.
- [ ] Tag the merged commit and verify all four official archives and hashes.
- [ ] Publish and verify crates.io, five npm packages and the Homebrew formula;
  verify the default shell installer. Do not republish an accepted version.
- [ ] Install the exact release on WSL and M3, regenerate managed launchers and
  restart only Swapdex services. Check running executable hashes/build markers.
- [ ] Verify installed disconnect/streaming behavior, account routing, native
  resume, and bounded live requests. Compare selection pointers and observe
  restart counts after normal operation.
- [ ] Append durable delivery results, including any failed/unavailable checks,
  to version-specific release and PR records and report the final commit/links.

## Verified source and delivery record

The final local tree passed 1,217 Rust tests (0 failed, 2 intentionally ignored),
Clippy, rustfmt, 22 Python tests and 7 npm tests. Both tools passed the installed
streaming/disconnect verifier against the candidate executable. Spec compliance
and code-quality review passed. The Linux picker fixture's parallel copy/spawn
race was reproduced in 2/12 trials and eliminated in 12/12 parallel trials by
protecting that preparation interval; the picker interactions remain concurrent.

Publication, CI and installed-service results are recorded in the PR and the
[version-specific release](https://github.com/youdie006/swapdex/releases/tag/v0.165.4).
Those records complete the delivery steps after this source snapshot is tagged.
