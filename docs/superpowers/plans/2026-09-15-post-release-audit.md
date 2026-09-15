# Post-release audit implementation plan

Base: `855e85c` (`v0.163.0`). Branch: `fix/post-release-audit`.
Spec: `../specs/2026-09-15-post-release-audit.md`.

- [x] Recheck installed 0.163.0 routing before extending the previous change.
- [x] Reproduce launcher relative paths, executable selection and rooted
  shell-profile writes using disposable homes.
- [x] Reproduce supervisor path encoding with generated units and
  `systemd-analyze --user verify`.
- [x] Reproduce separate Codex subjects sharing one refresh claim and lost
  reset-only quota telemetry.
- [x] Repair shim discovery/profile containment and generated swap commands.
- [x] Repair service encoding and verify both Linux units and macOS plists.
- [x] Integrate complete refresh identity into coordinator and manual dedup.
- [x] Coordinate mixed complete/opaque copies of one rotating token and retain
  generation-sensitive retry decisions.
- [x] Preserve quota reset windows until expiration and distinguish current
  slot usage from stale snapshots and unrelated native logins.
- [x] Repair account-filter ordering and directory traversal in native session
  lookup; verify timeout cleanup against the installed failing binary.
- [x] Repair provider-scoped failover, stale automatic choice writes and
  ambiguous accepted-POST retries; verify explicit-off and concurrent choices.
- [x] Review integrated diff and run repository checks plus installed-style
  routing and stock Codex session verifiers against the candidate.
- [ ] Record concrete changes, commit and push, complete CI and review.
- [ ] Publish a versioned release to GitHub, npm, crates.io and Homebrew.
- [ ] Install the exact version on WSL and M3; verify running proxies,
  unchanged selected accounts and installed routing/session behavior.
- [ ] Preserve publication and deployment evidence in release/PR records.

File ownership during implementation: main owns service, commands, records,
quota visibility and installed verification; launcher worker owns shim/run
fixtures before returning ownership; renewal worker owns refresh, quota cache,
native discovery and timeout fixtures; proxy worker owns proxy modules and
integration tests. Commands ownership transferred to main before manual dedup
integration. No parallel edits to the same file.
