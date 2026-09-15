# Contributing to swapdex

Thanks for your interest. swapdex is a small, security-sensitive tool; the bar
for changes that touch credential handling is high.

## Building

```sh
cargo test --all
cargo clippy --all-targets -- -D warnings
cargo fmt --all -- --check
```

All three must pass. Tests run against an isolated temp `HOME` (via
`Paths::rooted`) and never touch a real login -- keep it that way: any new path
resolution must go through `Paths`, never `dirs::home_dir()` directly.

**Manual runs during development must set `SWAPDEX_ROOT`:**

```sh
export SWAPDEX_ROOT=$(mktemp -d)   # every path resolves under this dir
cargo run -- status                # safe: reads the empty temp root
```

Without it, `cargo run -- use x` operates on YOUR real `~/.claude` and
`~/.codex` logins. Seed fake credentials the way the E2E tests do
(`seed_codex` / `seed_claude` in `tests/switch.rs` show the exact file shapes).

## The most useful contribution: a new tool adapter

swapdex supports Claude Code, Codex, Gemini CLI, and Antigravity today.
Adding another CLI (OpenCode, Cursor, ...) means implementing the `AuthTool`
trait in `src/adapters/`:

- `capture` reads the current live login into an opaque `Snapshot`.
- `apply` writes a snapshot back atomically (use `crate::atomic::write_secret`;
  for any file that mixes credentials with unrelated config, do a field-level
  read-modify-write like the Claude adapter, never a whole-file overwrite).
- `identity` returns a redacted `Account` -- never hold a token in a loggable
  field.

Include a capture/apply round-trip test against an isolated `Paths::rooted`.

macOS Keychain support for Claude Code shipped in 0.17-0.24 (see the Keychain
resolution contract in `src/adapters/claude.rs`); new-adapter and hardening
contributions are the most useful right now.

## Non-negotiables

- Keep the existing lightweight proxy transport (`ureq` with rustls and bundled
  roots). CI rejects heavy async runtimes, HTTP frameworks and system-TLS bindings.
- No command or MCP tool may print a credential, and none may switch accounts
  automatically. swapdex is a switcher, not a rotator.
- Do not add an `--auto`/`--next`/`--when-rate-limited` flag or a token-export
  command.

By contributing you agree your work is licensed under the MIT License.

## Releasing to the installation channels

The tag workflow builds GitHub release assets. It does not publish to npm or
crates.io; a successful GitHub release alone does not update those installers.

1. Keep the Cargo version, lockfile, npm version, pinned platform versions,
   changelog section, and generated man page in the release commit.
2. Run the checks above and the npm tests: `node --test 'npm/**/*.test.mjs'`.
3. Push the version tag and wait for all four GitHub binary builds to finish.
4. From `npm/`, run `node publish.mjs <version>`. This publishes the platform
   packages before the main package and checks that all five resolve on npm.
5. Publish the crate with `cargo publish` and verify the registry version.
6. Update the `youdie006/homebrew-tap` formula's version, all platform URLs,
   archive SHA-256 values and version assertion. Verify actual release downloads
   against the formula, check Ruby syntax, and record the merged tap PR. The tag
   workflow does not update Homebrew; do not leave that channel on an old release.
7. Install the exact npm version on the target machine, then compare
   `npm ls -g --depth=0 @youdie006/swapdex` with `swapdex --version` and the
   executable found by `command -v swapdex`. Running proxies keep their old
   executable until restarted; verify their version markers after applying
   the update.
8. Test the installed native executable, separately from the source build:
   `python3 scripts/verify-installed-account-routing.py --swapdex /absolute/path/to/native/swapdex`.
   This uses temporary fake accounts and loopback servers to verify generated
   launcher behavior and next-request account switching over one connection.
   It also accepts a POST at a fake provider and resets the connection before
   responding, verifying that neither tool submits that accepted turn again.
   Bodyless GET requests must still recover from the same connection failure.
   Also run `scripts/verify-codex-session-resume.py` with the installed native
   Swapdex and stock Codex paths to check native session listing/resume and
   WebSocket-to-HTTP recovery. Both scripts clean up their test processes.

A failed publication remains an incomplete release. Do not reuse an already
published version or replace an old tag to repair a missing channel.

Keep an auditable update record: user-visible fixes belong in `CHANGELOG.md`
with the concrete problem and resulting behavior, and commits and PRs must
describe their validation. A version bump or a generic "update" is not enough.
After publishing and installing, append the tag/commit, channel checks, target
installation version and running-service verification to the release or PR.
Distinguish a pushed source change from a published release and an installed
update; do not leave the only deployment record in chat or include credentials.
