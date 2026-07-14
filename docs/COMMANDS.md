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
| `swapdex ui` | Persistent full-screen UI (real terminal): the screen clears and the UI stays up. Arrow keys + Enter switches (result in the status line, list refreshes in place); after a switch - or with `o` - the conversation menu opens: recent sessions (resumed in their own folder; sessionwiki when installed, the tools' own stores otherwise) plus new-conversation entries with a folder prompt. `a` add account, `n` rename, `u` local usage, `%` remaining quota (the one networked panel), `?` health check, `r` restore, `d` delete, `s` save the current login (onboarding), `j`/`k` or the mouse wheel to move (text panels scroll with the wheel too), `q` quit. Opening a conversation is the one action that leaves. Needs a terminal (pipes are refused - script with `swapdex use`); a dumb terminal (`TERM=dumb`) gets a plain numbered prompt instead of the full-screen UI. |
| `swapdex onboard` | Guided setup: registers any existing `~/.claude-*` config dirs as accounts, moves legacy snapshot profiles onto their own slots, and offers to install the `claude` shim - one `[Y/n]` at a time. A bare `swapdex` runs this automatically the first time there is something to set up (shown once, then it drops into the picker). Non-interactive shells are never hijacked. |
| `swapdex run <name> [-- <args>]` | Launch Claude in `<name>`'s own permanent slot (`exec`s `claude` with that slot's `CLAUDE_CONFIG_DIR`). Creates the slot on first use (first launch = the tool's own sign-in; swapdex writes no credential). Concurrent-safe - each terminal can run a different account. Anything after `--` is passed straight to `claude`. |
| `swapdex use <name> [--tool ...] [--dry-run] [--open [--dir <path>]]` | If `<name>` is a permanent slot, repoints the default-account pointer that the `claude` shim follows (no credential copy, so it can never log you out). Otherwise switches a snapshot profile: backs up the current login first, refreshes the outgoing account's saved profile with its latest (possibly rotated) tokens, applies atomically, and is refused while a `claude` session runs on that login slot (`--force` overrides). `--dry-run` prints the change without writing. `--open` launches the tool right after a snapshot switch (needs `--tool`; `--dir` picks the folder). `use -` toggles to the previous/other profile; a unique prefix works too. |
| `swapdex shim` | Install the `claude` shim: a tiny launcher that reads the default-account pointer and runs the real `claude` in that slot, so a plain `claude` follows `swapdex use`. Prints the one `PATH` line to add. Re-run to refresh it. `doctor` reports whether it is installed and ahead of the real binary on `PATH`. |
| `swapdex slots` | List the permanent account slots (name and the `CLAUDE_CONFIG_DIR` each launches into). |
| `swapdex adopt <name> <dir>` | Register an existing `CLAUDE_CONFIG_DIR` directory (e.g. `~/.claude-work`) as an account, in place - not moved or copied. |
| `swapdex migrate` | Give each legacy snapshot Claude profile its own permanent slot, so it can be used via `run`/`use` with no credential copying. Does not import a token (the slot's login is a fresh sign-in); prints the accounts to log into once. Idempotent. |
| `swapdex ls [--json] [--names]` | List saved profiles with the account email, tier, and a `(expired)` / `(stale)` / `(unreadable)` marker. The active profile is marked from the **live** login, not a stored guess. `--names` prints bare names one per line (for scripts and completion). |
| `swapdex status [--json] [--short]` | Show the active account per tool, matched back to a saved profile, plus expiry and a session summary (needs sessionwiki). `--json` for scripting; `--short` prints one compact `claude:work codex:personal` line for shell prompts and statuslines. |
| `swapdex restore [--tool ...] [--dry-run]` | Put back the login that was live before the last switch (`use` backs it up first, even when it was never saved as a profile). Backs up the current login before applying, so running it again toggles back. |
| `swapdex rm <name> [--yes]` | Remove a saved profile. Asks y/N on a terminal; `--yes` skips the question (and is required when stdin is not a tty, e.g. scripts). Never touches a live login. |
| `swapdex rename <old> <new>` | Rename a saved profile. |
| `swapdex sessions [--json]` | Sessions grouped by the account active when they ran (best-effort; needs sessionwiki on PATH - the ui's session menu itself does NOT). |
| `swapdex usage [--json]` | Recent local token usage per tool over the last 5h and 7d, summed from `~/.claude` and `~/.codex` session logs - **per account** once a switch history exists (each event is attributed to the profile active at its timestamp; what predates your first switch shows as untagged). A rough activity gauge, not the billed quota. Reads local files only - never the network. |
| `swapdex quota [--json]` | Remaining balance per **Claude** account, live from Anthropic's OAuth usage endpoint (5h/7d windows, per-model weekly caps, reset countdowns). **The one opt-in network command**: it shells out to `/usr/bin/curl` with each account's own token - read-only, spends zero message quota, runs only when you type it. The active account uses its live token; a saved account whose snapshot token has expired reports so instead of showing a stale number. `--json` includes the raw response for any unexpected shape. Always exits 0. |
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
