# Changelog

All notable changes to swapdex are documented here. This project follows
[Semantic Versioning](https://semver.org) and
[Keep a Changelog](https://keepachangelog.com).

## [0.5.0] - 2026-07-07

Two headline features: a third tool, and per-account usage.

### Added
- **Gemini CLI support**: `~/.gemini/oauth_creds.json` +
  `~/.gemini/google_accounts.json` are switched together with the same
  both-or-neither rollback the Claude adapter uses. One profile can now hold
  Claude Code + Codex + Gemini and a single `use` switches all three;
  `--tool gemini` scopes any command; `ls`/`status`/`ui`/`doctor`/`restore`
  cover it like the others; sessionwiki's account badges pick Gemini sessions
  up automatically (the timeline join is tool-generic). `--tool all` is the
  explicit everything-selector (alias `both` kept for scripts).
- **`usage` is per-account once a switch history exists**: every token event
  is attributed to the profile active at its timestamp - the same honest join
  `sessions` uses - so "how much have I used on EACH account" finally has an
  answer. What predates your first switch shows as untagged; no history, no
  guessing. JSON grows an `accounts` object per tool.

## [0.4.2] - 2026-07-07

Ecosystem-walkthrough fixes: the integrated flows, from a fresh user's chair.

### Fixed
- The `ui` resume handoff passes `--no-sync`: on a large store the exec used
  to kick off a full index sync - minutes of progress spam that looked like a
  hang in the flagship flow. sessionwiki still self-syncs when the id is not
  yet indexed.
- A present-but-never-synced sessionwiki no longer reads as "0 sessions":
  `sessions` and `status` say "index empty - run `sessionwiki sync` once".
- The sessionwiki read cap rose 1000 -> 50000, so the status summary cannot
  silently understate a large store.

### Added
- `sessions --json`: {"available", "accounts", "total"} for scripting
  (available=false distinguishes "no sessionwiki" from "zero sessions").

## [0.4.1] - 2026-07-07

Fixes from an adversarial audit of the 0.4.0 delta.

### Fixed
- `ui` no longer panics on a session id with multibyte characters (the id
  prefix was a byte slice; now char-based).
- The "any account" continuity fallback now fires on the FIRST real switch -
  the very case it was written for. (The empty-timeline check ran after the
  switch had already written its own event, so it only ever fired on a no-op
  pick.)
- `exec` handoff passes the session id after a `--` separator, so an id that
  begins with `-` can never be parsed as a flag.
- The `SWAPDEX_SESSIONWIKI_JSON` test fixture hook is only honored together
  with `SWAPDEX_ROOT` - a stray env var can no longer redirect a production
  run.

### Docs
- The README demo now shows the full integrated loop: `ui` -> switch ->
  recent sessions -> resume handoff -> `status --short`.

## [0.4.0] - 2026-07-07

### Added
- `ui` completes the loop: pick a recent session by number after the switch
  and swapdex hands off to `sessionwiki resume <id>` directly (a one-shot
  `exec` of the official reopen flow - the session's own tool takes over the
  terminal). Enter skips; nothing ever launches unasked. This is the same
  precedent as `login` driving the official sign-in: an explicit hand-off is
  not a wrapper - swapdex `exec`s and is gone.

## [0.3.1] - 2026-07-07

### Added
- `ui` shows a continuity hint after the switch: the picked account's recent
  sessions (id, relative age, tool, title) with the one command to reopen one
  (`sessionwiki resume <id>`) - switch, land back in the work you switched
  for. Before the first recorded switch, when nothing can be attributed yet,
  it honestly falls back to the most recent sessions of any account and says
  so. Requires sessionwiki; silently absent otherwise.

## [0.3.0] - 2026-07-07

### Added
- `swapdex ui`: an interactive picker - every profile with its account,
  active marker, and the session summary; type a number to switch, Enter or
  `q` cancels. The selection runs the exact same safe `use` path (backup,
  validate, atomic apply), so a human picking a number IS the explicit
  switch - the no-auto-rotation bright line is untouched. Deliberately
  stdin-only: no raw-mode TUI crate, nothing socket-shaped enters the
  dependency graph.

## [0.2.2] - 2026-07-07

### Fixed
- `ls` aligns by display width, so a CJK profile name (two columns per
  character) no longer shears the table.

### Docs
- The `status --short` line drops straight into Claude Code's own status line
  (`statusLine` snippet in the README) - the active account stays visible
  inside the tool you are switching.
- An honest Alternatives section (claude-swap, aisw, caam) with each
  project's trade-offs and when to pick them over swapdex.

## [0.2.1] - 2026-07-06

Fixes from an adversarial audit of the 0.2.0 delta, plus scripting/completion
polish.

### Fixed
- `use ""` (an unset shell variable) matched a single profile as a "unique
  prefix" and performed a real switch; an empty name is now rejected (exit 2)
  with the live login untouched.
- `use -` can no longer re-pick the profile you are already on when the live
  identity is unreadable (the newest switch's destination is excluded); the
  refusal message says the real reason when both profiles are active; and
  `--tool` now scopes the `-` resolution.
- macOS Keychain-mode installs: a bare `use`/`restore` skips claude-code with
  a note and keeps switching Codex, instead of aborting the whole command.
- `doctor`: the store-permission check could never fire (the store self-heals
  its mode on open) - it now reports what it found; the expired/stale remedy
  says "log in to that account" first, so following it verbatim can no longer
  overwrite the profile with whatever account happened to be live.
- `rm` checks the profile exists before asking y/N.
- `manpage` failures exit 1 instead of printing nothing successfully.
- A legacy profile literally named `-` stays manageable after the upgrade
  (`-` is rejected only when creating/renaming).
- A bare `swapdex` no longer creates the store directory as a side effect.

### Added
- `ls --names`: bare profile names one per line; the docs gain a verified
  bash/zsh snippet that tab-completes profile names for `use`/`rm`/`rename`.
- `add` with no name asks on a terminal (name suggested from the live
  account); non-interactively it errors with the fix.
- `doctor` verdicts are colored on a TTY; NO_COLOR is respected everywhere.
- The demo GIF shows `use -`, `status --short`, and the colored doctor.

## [0.2.0] - 2026-07-06

Daily-driver ergonomics: the goal is a switch in two keystrokes and zero
guessing about where you stand.

### Added
- `swapdex use -`: toggle to the previous/other profile, like `cd -` /
  `git switch -`. With two profiles it is simply the other one; with more it
  is the profile you were on before (from the switch timeline). `-` is now a
  reserved name.
- Unique-prefix matching on `use`: `swapdex use w` resolves to `work` and says
  so; an ambiguous prefix refuses and lists the candidates (switching is a
  write - it never guesses).
- `swapdex status --short`: one compact `claude:work codex:personal` line for
  shell prompts and statuslines (starship/PS1 snippet in the README).
- A bare `swapdex` now shows the active accounts under the banner, so the
  naked command answers "where am I?".

### Changed
- `rm` asks y/N on a terminal instead of demanding `--yes`; scripts keep the
  explicit `--yes` requirement (exit 7 when stdin is not a tty).

## [0.1.9] - 2026-07-06

### Added
- `swapdex manpage`: prints the man page (roff) to stdout. Homebrew installs
  it - and shell completions - automatically.
- A demo GIF of the core loop (ls -> use -> status -> restore -> doctor) at
  the top of the README.

### Fixed
- `use`/`restore` no longer print the running-session warning under
  `SWAPDEX_ROOT`: an isolated root's credentials are not the ones a live
  session uses, so the warning was a false positive there.

## [0.1.8] - 2026-07-06

### Added
- `swapdex doctor`: local health check - store permissions, every saved
  snapshot (unreadable/expired/stale), both live logins (including the
  corrupt-file case), backups, `.claude.json` permissions, and the CLIs on
  PATH. Each finding ends with its remedy. Exit 0 healthy, 9 when problems
  were found. Local only, never the network.

### Changed
- The switch timeline file is bounded (compacts to the newest 1000 events)
  instead of growing forever.
- `add` hints about quoting when a profile name contains spaces.

## [0.1.7] - 2026-07-06

Findings from a two-track review (adversarial code audit + a new-user
walkthrough), all fixed and regression-tested.

### Added
- `swapdex restore [--tool ...] [--dry-run]`: put back the login that was live
  before the last switch. `use` has always backed up the outgoing login (even
  one never saved as a profile), but there was no command to bring it back - a
  bad switch meant hand-copying files. `restore` backs up the current login
  first, so running it again toggles between the two. A bare `restore` scopes
  itself to the tool(s) the last switch touched, and it skips a torn backup in
  favor of an older intact one.
- `use` warns when the OUTGOING login is not saved as any profile (only the
  last 2 backups remember it), and when a live session of the switched tool is
  running.

### Fixed
- `usage` was wrong in both directions: Codex was undercounted 10-100x (it
  read the per-request `last_token_usage` as if it were cumulative; now it
  windows the deltas of the monotonic `total_token_usage` by event time) and
  Claude was overcounted ~2.5x (one line per content block repeats the same
  `message.id`, and resumed sessions copy messages into new files; now deduped
  by message id). Also ~9x faster (streaming + pre-filter instead of
  whole-file reads; 12.2s -> 1.3s on a 927MB transcript set).
- A corrupt live credential file no longer blocks recovery: `use <profile>`
  warns and replaces it (previously it aborted - the one command that could
  fix the file refused to run), `status` reports "login file unreadable" per
  tool instead of dying mid-output, and `restore` tolerates it too.
- macOS: `use`/`add`/`restore` on a Keychain-mode Claude Code install now
  refuse with an explanation instead of half-switching (writing a credentials
  file the CLI ignores while flipping the reported identity). `status` and
  `add` explain the Keychain situation. Codex switching works on macOS.
- `rename` to an existing name exits 6 ("already exists", like `add`) instead
  of a generic hard error, and takes the store lock like every other mutation.
- `login <name> --tool claude` with no CLI on PATH exits 3 on stderr (was:
  exit 0 on stdout - scripts saw success where nothing was saved).
- A corrupt saved snapshot is now visible as `(unreadable)` in `ls` with a
  remedy footer, and a failing `use` names the profile and the fix.
- Claude apply: if the rollback after a failed config write ALSO fails (e.g.
  disk full), the error now says so instead of claiming a clean rollback.
- `restore` attributes its timeline event to the restored profile's name, so
  `sessions` no longer blames an account literally named "(backup)".
- setup: Ctrl-D (EOF) at a prompt exits cleanly instead of spinning forever.
- Timestamps with fractional seconds AND a numeric timezone offset
  ("...00.123+09:00") now parse the offset instead of ignoring it.

### Changed
- `use` prints "{tool}: profile 'x' has no {tool} login - left unchanged" when
  a logged-in tool is skipped, instead of silently half-switching; `--dry-run`
  shows the target account's email.
- `ls` aligns by characters (not bytes) and truncates over-long names/emails
  with an ellipsis so one long row cannot shear the table (full values in
  `--json`).
- `status --json` has a stable shape: every key present on every row (null
  when unknown) plus an `unreadable` flag.
- Parent directories swapdex creates for credential files (e.g. a fresh
  `~/.codex`) are 0700, not umask-default.

## [0.1.6] - 2026-07-06

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
