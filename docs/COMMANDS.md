# swapdex commands

A quick reference. Run `swapdex --help` for the generated help. Bare
`swapdex` (no arguments) opens the full-screen picker when run on a terminal
with saved profiles or a live login; otherwise it prints the wordmark and a
short hint.

## Commands

| Command | What it does |
| --- | --- |
| `swapdex setup` | Guided first-time setup: saves the account(s) you're logged into, offers to add more (drives `codex login` for you), and shows how to switch. Interactive (needs a terminal). |
| `swapdex login <name> [--tool ...]` | Log in to a NEW account and save it, in one flow. Already logged in? swapdex saves your current login, signs you out locally, opens the official tool for the fresh sign-in, and captures the new account - your previous login is stashed and restored automatically if the sign-in does not complete. Without `--tool` it asks which tool (never guesses). |
| `swapdex add <name> [--tool ...] [--update]` | Save the current live login as a named profile. Snapshots both tools by default; `--tool` limits it. `--update` replaces an existing snapshot. |
| `swapdex ui` | Persistent full-screen UI (real terminal): the screen clears and the UI stays up. Arrow keys + Enter switches (result in the status line, list refreshes in place); after a switch - or with `o` - the conversation menu opens: recent sessions (resumed in their own folder; sessionwiki when installed, the tools' own stores otherwise) plus new-conversation entries with a folder prompt. `a` add account, `n` rename, `u` local usage, `%` remaining quota (the one networked panel), `?` health check, `r` previous account (`use -`), `d` delete, `s` save the current login (onboarding), `j`/`k` or the mouse wheel to move (text panels scroll with the wheel too), `q` quit. Opening a conversation is the one action that leaves. Needs a terminal (pipes are refused - script with `swapdex use`); a dumb terminal (`TERM=dumb`) gets a plain numbered prompt instead of the full-screen UI. |
| `swapdex proxy` | Change accounts inside a conversation that is ALREADY running, and optionally continue on another account when one runs out. Runs a loopback HTTP server (`--port`, default 8787) that forwards Claude's API traffic upstream, choosing the account per request: `swapdex use <name>` (or Enter in `swapdex ui`) moves the conversation you are in the middle of - no new chat, no `--resume`. With `--auto`, an account that reports itself spent hands the session to another one by itself, at the turn boundary so a completed answer is never cut off. `--account <name>` pins one account for every request instead. With the `claude` shim installed a plain `claude` picks up a running proxy automatically; otherwise export the `ANTHROPIC_BASE_URL` line it prints. Tokens are read from each account's own slot and never copied; only slot accounts are eligible (`swapdex run <name>` signs one in). Binds 127.0.0.1 only, and `claude` must run in the same environment as the proxy (inside WSL2 the loopback is WSL's own). The log line carries the account, path and status - never a body, never a token. |
| `swapdex onboard` | Guided setup: registers any existing `~/.claude-*` config dirs as accounts, moves legacy snapshot profiles onto their own slots, and offers to install the `claude` shim - one `[Y/n]` at a time. A bare `swapdex` runs this automatically the first time there is something to set up (shown once, then it drops into the picker). Non-interactive shells are never hijacked. |
| `swapdex run <name> [--tool ...] [-- <args>]` | Launch a tool in `<name>`'s own permanent slot (`exec`s `claude` with that slot's `CLAUDE_CONFIG_DIR`). Creates the slot on first use (first launch = the tool's own sign-in; swapdex writes no credential). Concurrent-safe - each terminal can run a different account. Anything after `--` is passed straight to `claude`. |
| `swapdex use <name> [--tool ...] [--dry-run] [--open [--dir <path>]]` | If `<name>` is a permanent slot, repoints the default-account pointer that the `claude` shim follows (no credential copy, so it can never log you out). Otherwise switches a snapshot profile: backs up the current login first, refreshes the outgoing account's saved profile with its latest (possibly rotated) tokens, applies atomically, and is refused while a `claude` session runs on that login slot (`--force` overrides). `--dry-run` prints the change without writing. `--open` launches the tool right after a snapshot switch (needs `--tool`; `--dir` picks the folder). `use -` toggles to the previous/other profile; a unique prefix works too. |
| `swapdex shim` | Install the shims (`claude`, and `codex` when it is on PATH): a tiny launcher that reads the default-account pointer and runs the real `claude` in that slot, so a plain `claude` follows `swapdex use`. Prints the one `PATH` line to add. Re-run to refresh it. `doctor` reports whether it is installed and ahead of the real binary on `PATH`. |
| `swapdex slots` | List the permanent account slots (name and the `CLAUDE_CONFIG_DIR` each launches into). |
| `swapdex adopt <name> <dir>` | Register an existing `CLAUDE_CONFIG_DIR` directory (e.g. `~/.claude-work`) as an account, in place - not moved or copied. |
| `swapdex migrate [--tool claude\|codex]` | Match saved Claude and Codex profiles to slots by account. Creates slots only for accounts without one, reports profiles that are copies of differently named slots, and reports unreadable snapshots without guessing. Does not import a token; each created slot needs one fresh sign-in. Idempotent, including before that sign-in. |
| `swapdex sync-mcp` | Copy the `mcpServers` block from `~/.claude.json` into every slot's own `.claude.json`, preserving each slot's `oauthAccount`. `settings.json` and global `CLAUDE.md` are symlinked into new slots automatically, but MCP config is mixed with the per-account identity in `.claude.json`, so it is shared with this explicit merge. Run it after logging into your slots (a slot has no `.claude.json` until first login). |
| `swapdex ls [--json] [--names]` | List saved profiles with the account email, tier, and a `(expired)` / `(stale)` / `(unreadable)` marker. The active account is marked from the **pointer** a switch sets, falling back to the live login where no slot points anywhere; a login left in the tool's own dir gets its own line rather than being shown as active. `--names` prints bare names one per line (for scripts and completion). |
| `swapdex status [--json] [--short]` | Show the active account per tool, matched back to a saved profile, plus expiry and a session summary (needs sessionwiki). `--json` for scripting; `--short` prints one compact `claude:work codex:personal` line for shell prompts and statuslines. |
| `swapdex restore [--tool ...] [--dry-run]` | Put back the login that was live before the last switch (`use` backs it up first, even when it was never saved as a profile). Backs up the current login before applying, so running it again toggles back. |
| `swapdex rm <name> [--yes]` | Remove a saved profile. Asks y/N on a terminal; `--yes` skips the question (and is required when stdin is not a tty, e.g. scripts). Never touches a live login. |
| `swapdex rename <old> <new>` | Rename a saved profile. |
| `swapdex sessions [--json]` | Sessions grouped by the account active when they ran (best-effort; needs sessionwiki on PATH - the ui's session menu itself does NOT). |
| `swapdex usage [--json]` | Recent local token usage per tool over the last 5h and 7d, summed from `~/.claude` and `~/.codex` session logs - **per account** once a switch history exists (each event is attributed to the profile active at its timestamp; what predates your first switch shows as untagged). A rough activity gauge, not the billed quota. Reads local files only - never the network. |
| `swapdex quota [--json]` | Remaining balance per **Claude** account, live from Anthropic's OAuth usage endpoint (5h/7d windows, per-model weekly caps, reset countdowns). **The one opt-in network command**: it shells out to `/usr/bin/curl` with each account's own token - read-only, spends zero message quota, runs only when you type it. The active account uses its live token; a saved account whose snapshot token has expired reports so instead of showing a stale number. `--json` includes the raw response for any unexpected shape. Always exits 0. |
| `swapdex serve [<name>] [--tool ...] [--off] [--quiet]` | Hand turns to an account without moving where new sessions start. Two pointers on purpose: `serve` decides who PAYS, `use` decides where new conversations LIVE. Bare `serve` shows who is serving; `--off` lets each session pay for itself again. `--quiet` prints one line for a status bar: the account, its login, and what it has left, from cache. |
| `swapdex refresh [<name>] [--keep-alive]` | Renew an access token whose hour has lapsed, in place, without a sign-in. `--keep-alive` sweeps every account heading for expiry instead of only the lapsed ones - a refresh token goes stale when it is not exercised, and an idle slot dies about ten days after its last run. Renewals are keyed by ACCOUNT: two slots holding one login are renewed once, because these tokens are single-use and spending one twice is what logs an account out. |
| `swapdex service <install\|uninstall\|status> [--tool ...]` | Keep the proxy running as a launchd/systemd service, per tool. This is also what puts renewals on a timer: the sweep lives in the proxy, so without a service nothing is renewing anything. |
| `swapdex pause <name>` / `swapdex resume <name>` | Keep an account out of the proxy's automatic rotation, or put it back. `use` and `serve` still reach it by name - this is only about what the proxy picks on its own. `ls` marks the row `(rotation paused)` and `ls --json` carries `"paused"`. |
| `swapdex whereis [<project>]` | Find which account holds a conversation, searching every account's store. Prints the command that resumes it, per account. |
| `swapdex share-history [--tool ...] [--dry-run]` | One-time repair: make every conversation reachable from every account. Links each slot's conversation store to the shared one and carries over anything only that slot had. A slot holding its own store is reported and left alone rather than linked over. |
| `swapdex slash` | Install a `/swap` command for Claude Code (`~/.claude/commands/swap.md`) and a matching skill for Codex, so an account can be switched without leaving the chat. |
| `swapdex export <file>` / `swapdex import <file>` | Write this machine's account setup - names and settings, **never a login** - and re-create it elsewhere. Accounts already present are left alone, and each still needs its own sign-in. |
| `swapdex auto <on\|off>` | Whether the proxy hands a spent session to another account by itself. Read on every request, so it reaches a proxy that is already running. |
| `swapdex strategy <roomiest\|consume-first>` | Which account auto-continue reaches for: the one with the most headroom, or the current one until it is spent. |
| `swapdex threshold <value\|off>` | Step off an account at this much used (`0.9` or `90%`). Opt-in: it costs one usage read per account, so without it the proxy originates no traffic of its own. |
| `swapdex fallback-model <model\|off>` | A cheaper model to ask for when every account is past the threshold. Off by default, and only used when there is nowhere else to go. |
| `swapdex doctor` | Local health check: store permissions, every saved snapshot, every live login, backups, the CLIs on PATH, and (macOS) whether the Claude Keychain item swapdex resolves matches this environment - the classic "my switch didn't stick" cause - each finding ends with its fix. Exit 0 healthy, 9 when problems were found. Never touches the network. |
| `swapdex mcp` | Run as a read-only MCP server over stdio (`whoami`, `list_accounts`). No switch tool exists. |
| `swapdex completions <shell>` | Print a tab-completion script for `bash`, `zsh`, `fish`, `elvish`, or `powershell`. This completes swapdex's own commands; it does not wrap or intercept `claude`/`codex`. Installed automatically by Homebrew. |
| `swapdex manpage` | Print the man page (roff) to stdout: `swapdex manpage > /usr/local/share/man/man1/swapdex.1`. Installed automatically by Homebrew. |

## Exit codes

| Code | Meaning |
| --- | --- |
| `0` | Success (including a no-op such as switching to the already-active account). |
| `1` | A hard error (message is redacted of home paths). |
| `2` | Invalid usage (bad flags/arguments, `ui` without a terminal) or an invalid profile name (`/`, `\`, leading `.`, control chars, >64 bytes). |
| `3` | Not logged in to the selected tool (`add`/`login`); or `login` over a pipe while already logged in (guidance only - nothing was saved). |
| `4` | The store is locked - another `swapdex` is mid-switch. |
| `5` | No profile by that name (`use` / `rm` / `rename`), or no backup (`restore`). |
| `6` | The profile already has a snapshot for that tool; pass `--update` (`add`); or the target name already exists (`rename`). |
| `7` | `rm` was called without `--yes`; or `add --update` refused to repoint a profile to a DIFFERENT account (repointing must be explicit). |
| `8` | `login` was started but the tool's login flow did not complete. |
| `9` | `doctor` found at least one problem. |

## Tools

`--tool` accepts `claude` (Claude Code; alias `claude-code`), `codex`,
`gemini`, `antigravity`, or `all` (alias `both`). With no `--tool` (same as
`all`), a command applies to whichever tools are relevant. The tool names in
output are `claude-code`, `codex`, `gemini`, and `antigravity`.

## Environment

| Variable | Effect |
| --- | --- |
| `CLAUDE_CONFIG_DIR` | Relocates Claude Code's config dir (honored, same as the CLI). |
| `CODEX_HOME` | Relocates Codex's home dir (honored, same as the CLI). |
| `SWAPDEX_ROOT` | Dev/test override: resolves every path (Claude, Codex, and the store) under one directory. Used by the test suite so tests never touch a real login. |
| `CLAUDE_SECURESTORAGE_CONFIG_DIR` | Read, not set. It decides which Keychain item a Claude session uses, so it is half of "which login slot is this running session on" - the question the switch guard answers before it refuses to swap a login out from under a live session. An environment that cannot be read makes that slot UNKNOWN and the guard fails closed. |
| `NO_COLOR` | Set it to anything and swapdex prints no ANSI colour, on a terminal or off one. The usual convention. |
| `SWAPDEX_TIMING` | Dev diagnostic: `swapdex ui` prints milestone timings while it starts. Nothing else reads it. |
| `HOME` | The base for `~/.claude.json`, `~/.claude/`, `~/.codex/`, `~/.gemini/`, and the store when the above are unset. |

## Tab-completing profile names

`swapdex completions <shell>` covers commands and flags. Profile names are
runtime data, so completing them takes one extra snippet (uses `ls --names`):

```sh
# bash (~/.bashrc)
_swapdex_profiles() {
  local cur=${COMP_WORDS[COMP_CWORD]}
  case "${COMP_WORDS[1]}" in
    use|rm|rename) COMPREPLY=($(compgen -W "$(swapdex ls --names 2>/dev/null)" -- "$cur")) ;;
  esac
}
complete -o default -F _swapdex_profiles swapdex
```

```sh
# zsh (~/.zshrc, after compinit)
_swapdex_profiles() {
  if (( CURRENT >= 3 )) && [[ ${words[2]} == (use|rm|rename) ]]; then
    compadd -- $(swapdex ls --names 2>/dev/null)
  fi
}
compdef _swapdex_profiles swapdex
```

## Where things live

- Store: `~/.local/share/swapdex/` on Linux, `~/Library/Application
  Support/swapdex/` on macOS (mode 0700) - named profile snapshots, a switch
  `timeline.jsonl`, and the last 2 backups per tool (taken by `use`/`restore`,
  read back by `restore`). It holds plaintext refresh tokens; protect it like
  `~/.ssh` and do not sync it.
- Claude Code login: `~/.claude/.credentials.json` plus the `oauthAccount` block
  inside `~/.claude.json` (only that block is swapped).
- Codex login: `~/.codex/auth.json`.
- Gemini CLI login: `~/.gemini/oauth_creds.json` plus
  `~/.gemini/google_accounts.json` (swapped together).
- Antigravity login: `~/.gemini/antigravity-cli/antigravity-oauth-token`.
