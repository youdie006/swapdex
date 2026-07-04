# swapdex commands

A quick reference. Run `swapdex --help` for the generated help, or `swapdex`
(no arguments) for the wordmark and a short hint.

## Commands

| Command | What it does |
| --- | --- |
| `swapdex add <name> [--tool claude\|codex] [--update]` | Save the current live login as a named profile. Snapshots both tools by default; `--tool` limits it. `--update` replaces an existing snapshot. |
| `swapdex use <name> [--tool ...] [--dry-run]` | Switch the active login to a saved profile. Backs up the current login first, then applies atomically. `--dry-run` prints the change without writing. Switching to the already-active account is a no-op. |
| `swapdex ls [--json]` | List saved profiles with the account email, tier, and a `(expired)` / `(stale)` marker. The active profile is marked from the **live** login, not a stored guess. |
| `swapdex status [--json]` | Show the active account per tool, matched back to a saved profile, plus expiry and a session summary (needs sessionwiki). `--json` for scripting. |
| `swapdex rm <name> --yes` | Remove a saved profile. Requires `--yes`. Never touches a live login. |
| `swapdex rename <old> <new>` | Rename a saved profile. |
| `swapdex sessions` | Sessions grouped by the account active when they ran (best-effort; needs sessionwiki on PATH). |
| `swapdex mcp` | Run as a read-only MCP server over stdio (`whoami`, `list_accounts`). No switch tool exists. |
| `swapdex completions <shell>` | Print a tab-completion script for `bash`, `zsh`, `fish`, `elvish`, or `powershell`. This completes swapdex's own commands; it does not wrap or intercept `claude`/`codex`. |

## Exit codes

| Code | Meaning |
| --- | --- |
| `0` | Success (including a no-op such as switching to the already-active account). |
| `1` | A hard error (message is redacted of home paths). |
| `2` | Invalid profile name (contains `/`, `\`, `..`, a leading `.`, or control chars). |
| `3` | Not logged in to the selected tool (`add`). |
| `4` | The store is locked - another `swapdex` is mid-switch. |
| `5` | No profile by that name (`use` / `rm` / `rename`). |
| `6` | The profile already has a snapshot for that tool; pass `--update` (`add`). |
| `7` | `rm` was called without `--yes`. |

## Tools

`--tool` accepts `claude` (Claude Code) or `codex`. With no `--tool`, a command
applies to whichever tools are relevant (both when present). The tool names in
output are `claude-code` and `codex`.

## Environment

| Variable | Effect |
| --- | --- |
| `CLAUDE_CONFIG_DIR` | Relocates Claude Code's config dir (honored, same as the CLI). |
| `CODEX_HOME` | Relocates Codex's home dir (honored, same as the CLI). |
| `SWAPDEX_ROOT` | Dev/test override: resolves every path (Claude, Codex, and the store) under one directory. Used by the test suite so tests never touch a real login. |
| `HOME` | The base for `~/.claude.json`, `~/.claude/`, `~/.codex/`, and the store when the above are unset. |

## Where things live

- Store: `~/.local/share/swapdex/` (mode 0700) - named profile snapshots, a
  switch `timeline.jsonl`, an `active.json` name hint, and bounded backups. It
  holds plaintext refresh tokens; protect it like `~/.ssh` and do not sync it.
- Claude Code login: `~/.claude/.credentials.json` plus the `oauthAccount` block
  inside `~/.claude.json` (only that block is swapped).
- Codex login: `~/.codex/auth.json`.
