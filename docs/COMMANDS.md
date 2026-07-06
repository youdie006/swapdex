# swapdex commands

A quick reference. Run `swapdex --help` for the generated help, or `swapdex`
(no arguments) for the wordmark and a short hint.

## Commands

| Command | What it does |
| --- | --- |
| `swapdex setup` | Guided first-time setup: saves the account(s) you're logged into, offers to add more (drives `codex login` for you), and shows how to switch. Interactive (needs a terminal). |
| `swapdex login <name> [--tool claude\|codex]` | Log in and save in one step. Codex: runs `codex login` (browser) then saves. Claude: if you're not logged in it opens Claude Code to sign in then auto-saves; if you already are, it guides the `add` step (Claude has no login CLI, so a re-login to a different account is done inside the app). |
| `swapdex add <name> [--tool claude\|codex] [--update]` | Save the current live login as a named profile. Snapshots both tools by default; `--tool` limits it. `--update` replaces an existing snapshot. |
| `swapdex use <name> [--tool ...] [--dry-run]` | Switch the active login to a saved profile. Backs up the current login first, then applies atomically. `--dry-run` prints the change without writing. Switching to the already-active account is a no-op. |
| `swapdex ls [--json]` | List saved profiles with the account email, tier, and a `(expired)` / `(stale)` marker. The active profile is marked from the **live** login, not a stored guess. |
| `swapdex status [--json]` | Show the active account per tool, matched back to a saved profile, plus expiry and a session summary (needs sessionwiki). `--json` for scripting. |
| `swapdex restore [--tool ...] [--dry-run]` | Put back the login that was live before the last switch (`use` backs it up first, even when it was never saved as a profile). Backs up the current login before applying, so running it again toggles back. |
| `swapdex rm <name> --yes` | Remove a saved profile. Requires `--yes`. Never touches a live login. |
| `swapdex rename <old> <new>` | Rename a saved profile. |
| `swapdex sessions` | Sessions grouped by the account active when they ran (best-effort; needs sessionwiki on PATH). |
| `swapdex usage [--json]` | Recent local token usage per tool over the last 5h and 7d, summed from `~/.claude` and `~/.codex` session logs. A rough machine-wide activity gauge (not tagged by account, not the billed quota) so you can tell when to switch. Reads local files only - never the network. |
| `swapdex doctor` | Local health check: store permissions, every saved snapshot, both live logins, backups, and the CLIs on PATH - each finding ends with its fix. Exit 0 healthy, 9 when problems were found. Never touches the network. |
| `swapdex mcp` | Run as a read-only MCP server over stdio (`whoami`, `list_accounts`). No switch tool exists. |
| `swapdex completions <shell>` | Print a tab-completion script for `bash`, `zsh`, `fish`, `elvish`, or `powershell`. This completes swapdex's own commands; it does not wrap or intercept `claude`/`codex`. Installed automatically by Homebrew. |
| `swapdex manpage` | Print the man page (roff) to stdout: `swapdex manpage > /usr/local/share/man/man1/swapdex.1`. Installed automatically by Homebrew. |

## Exit codes

| Code | Meaning |
| --- | --- |
| `0` | Success (including a no-op such as switching to the already-active account). |
| `1` | A hard error (message is redacted of home paths). |
| `2` | Invalid profile name (contains `/`, `\`, `..`, a leading `.`, or control chars). |
| `3` | Not logged in to the selected tool (`add`). |
| `4` | The store is locked - another `swapdex` is mid-switch. |
| `5` | No profile by that name (`use` / `rm` / `rename`), or no backup (`restore`). |
| `6` | The profile already has a snapshot for that tool; pass `--update` (`add`). |
| `7` | `rm` was called without `--yes`. |
| `8` | `login` was started but the tool's login flow did not complete. |
| `9` | `doctor` found at least one problem. |

## Tools

`--tool` accepts `claude` (Claude Code; alias `claude-code`), `codex`, or
`both`. With no `--tool` (same as `both`), a command applies to whichever tools
are relevant (both when present). The tool names in output are `claude-code`
and `codex`.

## Environment

| Variable | Effect |
| --- | --- |
| `CLAUDE_CONFIG_DIR` | Relocates Claude Code's config dir (honored, same as the CLI). |
| `CODEX_HOME` | Relocates Codex's home dir (honored, same as the CLI). |
| `SWAPDEX_ROOT` | Dev/test override: resolves every path (Claude, Codex, and the store) under one directory. Used by the test suite so tests never touch a real login. |
| `HOME` | The base for `~/.claude.json`, `~/.claude/`, `~/.codex/`, and the store when the above are unset. |

## Where things live

- Store: `~/.local/share/swapdex/` on Linux, `~/Library/Application
  Support/swapdex/` on macOS (mode 0700) - named profile snapshots, a switch
  `timeline.jsonl`, and the last 2 backups per tool (taken by `use`/`restore`,
  read back by `restore`). It holds plaintext refresh tokens; protect it like
  `~/.ssh` and do not sync it.
- Claude Code login: `~/.claude/.credentials.json` plus the `oauthAccount` block
  inside `~/.claude.json` (only that block is swapped).
- Codex login: `~/.codex/auth.json`.
