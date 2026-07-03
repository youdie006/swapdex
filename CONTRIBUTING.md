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

## The most useful contribution: a new tool adapter

swapdex supports Claude Code and Codex today. Adding another CLI (Gemini,
OpenCode, ...) means implementing the `AuthTool` trait in `src/adapters/`:

- `capture` reads the current live login into an opaque `Snapshot`.
- `apply` writes a snapshot back atomically (use `crate::atomic::write_secret`;
  for any file that mixes credentials with unrelated config, do a field-level
  read-modify-write like the Claude adapter, never a whole-file overwrite).
- `identity` returns a redacted `Account` -- never hold a token in a loggable
  field.

Include a capture/apply round-trip test against an isolated `Paths::rooted`.

## Non-negotiables

- No HTTP client or any network dependency may enter the graph. CI enforces
  this.
- No command or MCP tool may print a credential, and none may switch accounts
  automatically. swapdex is a switcher, not a rotator.
- Do not add an `--auto`/`--next`/`--when-rate-limited` flag or a token-export
  command.

By contributing you agree your work is licensed under the MIT License.
