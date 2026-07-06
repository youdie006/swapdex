# Changelog

All notable changes to swapdex are documented here. This project follows
[Semantic Versioning](https://semver.org) and
[Keep a Changelog](https://keepachangelog.com).

## [Unreleased]

### Added
- `swapdex usage [--json]`: recent local token usage per tool over the last 5h
  and 7d, summed from `~/.claude` and `~/.codex` session logs. A machine-wide
  activity gauge (not tagged by account, not the billed quota) so you can tell
  when to switch. Reads local files only - never the network, keeping the
  switcher-not-rotator stance intact.
- `use` now warns (best-effort) when the tool being switched has a live session
  running: a running session holds the old token and can overwrite the login
  you just switched to on its next refresh, so it prints a note to restart it.
  Detection is an exact process-name match (never a false alarm from a stray
  path), local only.

## [0.1.5] - 2026-07-06

### Changed
- `swapdex login --tool claude` now drives the flow instead of only printing
  guidance: if you are not logged in it opens Claude Code so you can sign in and
  auto-captures the result; if you already are, it guides the add/switch step.
  (Claude Code has no login subcommand, so a re-login to a different account is
  done inside the app.)

## [0.1.4] - 2026-07-06

### Added
- Guided onboarding: `swapdex setup` (interactive wizard - saves the accounts
  you are logged into, offers to add more, shows how to switch) and
  `swapdex login <name>` (log in and save in one step). The empty state and the
  no-argument banner now point new users to `swapdex setup`.

### Fixed
- `login`/`setup` back up the current Codex login before running `codex login`
  (which deletes `~/.codex/auth.json`), so an interrupted login is never lost.

## [0.1.3] - 2026-07-04

### Fixed
- `ls`, `status`, and the MCP `list_accounts` track the active account per tool
  (`active_tools`), fixing a mixed cross-tool state that marked both profiles
  active with a bare `*`.
- Removed the dead `active.json` hint - the live login drives every marker, so
  this dropped per-switch fsync churn and a corrupt-file surface.
- `ls` uses two-pass column widths and falls back to the tier when an email is
  missing (no stray leading-space `[tier]`).
- `session_link` skips the sessionwiki shell-out under `SWAPDEX_ROOT` so an
  isolated run never reads the host's real sessions.

## [0.1.2] - 2026-07-03

### Fixed
- `--tool` is a strict value set: a typo (`--tool cluade`) is rejected with the
  possible values instead of silently falling through to both tools.
- `use` no longer reports "already active" when the account id is empty, which
  could have kept the wrong account.
- `ls`/`status` inspect all of a profile's tools; Codex identity and the
  stale/expired marker were previously hidden behind the alphabetically-first
  `claude-code`.
- `add` (default) attaches a newly-available tool without forcing `--update`.

### Added
- The npm package ships its README (was blank on npmjs).

## [0.1.1] - 2026-07-03

### Added
- Shell completions: `swapdex completions <bash|zsh|fish|...>`.
- `status --json` for scripting.
- `ls` marks a saved login `(expired)` (Claude) or `(stale)` (Codex) so you know
  to re-capture it.

## [0.1.0] - 2026-07-03

### Added
- Initial release. Switch between multiple Claude Code and Codex login accounts
  locally: `add`, `use`, `ls`, `status`, `rm`, `rename`, `sessions`, and a
  read-only `mcp` server. In-place credential file swap, hardened for safety
  (0600 files, back-up-then-apply, symlink/root refusal, atomic writes, and a
  build-enforced no-network guarantee). Distributed via crates.io, Homebrew,
  npm, and prebuilt release binaries.
