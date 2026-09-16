# swapdex commands

A quick reference. Run `swapdex --help` for the generated help. Bare
`swapdex` (no arguments) opens the full-screen picker when run on a terminal
with saved profiles or a live login; otherwise it prints the wordmark and a
short hint.

## Commands

| Command | What it does |
| --- | --- |
| `swapdex setup` | Interactive guide to save existing live logins as snapshot profiles and add more. It does not by itself put existing native sessions behind the proxy. For separate Claude/Codex slots and next-request switching, follow the [quickstart](../README.md#quick-start). |
| `swapdex login <name> [--tool ...]` | Log in to a NEW account and save it, in one flow. Already logged in? swapdex saves your current login, signs you out locally, opens the official tool for the fresh sign-in, and captures the new account - your previous login is stashed and restored automatically if the sign-in does not complete. Without `--tool` it asks which tool (never guesses). |
| `swapdex add <name> [--tool ...] [--update]` | Save the current live login as a named snapshot profile, for each relevant tool by default. `--tool` limits it; `--update` replaces an existing snapshot. This captures the login already present; a second name does not create or sign into a different account. |
| `swapdex ui` | Persistent full-screen UI (real terminal): the screen clears and the UI stays up. Arrow keys + Enter switches (result in the status line, list refreshes in place); after a switch - or with `o` - the conversation menu opens: recent sessions (resumed in their own folder; sessionwiki when installed, the tools' own stores otherwise) plus new-conversation entries with a folder prompt. `l` sign this account in (launches its slot so the tool's own login runs there - the native tool writes the credential), `e` keep it out of the proxy's automatic rotation or put it back (the same setting `swapdex pause`/`resume` write), `a` add account, `n` rename, `u` local usage, `%` detailed quota (the account rows also fetch live usage), `?` health check, `r` previous account (`use -`), `d` delete, `s` save the current login (onboarding), `j`/`k` or the mouse wheel to move (text panels scroll with the wheel too), `q` quit. Opening a conversation is the one action that leaves. Needs a terminal (pipes are refused - script with `swapdex use`); a dumb terminal (`TERM=dumb`) gets a plain numbered prompt instead of the full-screen UI. |
| `swapdex proxy` | Run a loopback proxy for managed Claude or Codex requests (`--tool claude` by default; `--tool codex` selects Codex). A conversation must already route through this proxy for `swapdex serve <name> --tool ...` to affect its next request. A request in progress keeps its starting account. `--auto` permits configured fallback after account rejection/exhaustion; `--no-auto` disables it for this run. `--account <name>` pins an account while managed serving is enabled. `serve --off` overrides the pin and passes through client authentication. Shims start or reuse the proxy automatically and stop if startup fails. Proxy and native CLI must run in the same machine/WSL environment. Logs exclude request bodies and credentials. |
| `swapdex onboard` | Register existing `~/.claude-*` folders, offer migration of saved Claude/Codex accounts to slots, and offer missing shims for installed native tools. A new migrated slot still needs native sign-in. Bare `swapdex` offers this once when an installed client needs its shim, or discoverable folders/unmigrated profiles exist. Non-interactive input skips prompts. See the [quickstart](../README.md#quick-start) for the complete new-account flow. |
| `swapdex run <name> [--tool ...] [-- <args>]` | Create or open a permanent account slot and execute the native tool directly in its home (`--tool claude` by default). `--no-launch` only creates it. Use `swapdex run work --tool codex -- login --device-auth` or `swapdex run work --tool claude -- auth login` to sign into that slot. Arguments after `--` reach the native tool. A direct named run serves itself; use a plain shimmed `claude`/`codex` launch for proxy account switching. |
| `swapdex use <name> [--tool ...] [--dry-run] [--open [--dir <path>]]` | For a slot, select the home used by future shimmed launches and enable managed serving for that choice without copying credentials. Existing conversation homes stay put. Otherwise apply a saved snapshot with a backup and running-session guards (`--force` overrides the guard). `--dry-run` writes nothing. `--open` opens the selected tool after a successful switch (requires `--tool`); `--dir` chooses its folder. `use -` selects the previous/other account and a unique prefix is accepted. |
| `swapdex shim` | Install or refresh each available Claude/Codex launcher independently; neither native CLI requires the other. Report when no supported native executable is available. Update the shell profile when supported and print PATH guidance for each installed tool. Activate that change in the current shell or open a new terminal, then launch plain `claude`/`codex`. A native session started before this setup needs one relaunch through the shim to use the proxy. |
| `swapdex slots` | List permanent account slots and their configuration directories. The `*` marker identifies the default home for a plain shimmed launch. A created slot may still need sign-in; `ls`/`doctor` show login state. |
| `swapdex adopt <name> <dir> [--tool claude\|codex]` | Register an existing separate account directory in place, without moving it or copying its login. Defaults to Claude; use `--tool codex` for e.g. `~/.codex-work`. |
| `swapdex migrate [--tool claude\|codex]` | Match saved Claude and Codex profiles to slots by account. Creates slots only for accounts without one, reports profiles that are copies of differently named slots, and reports unreadable snapshots without guessing. Does not import a token; each created slot needs one fresh sign-in. Idempotent, including before that sign-in. |
| `swapdex sync-mcp` | Copy the `mcpServers` block from `~/.claude.json` into every slot's own `.claude.json`, preserving each slot's `oauthAccount`. `settings.json` and global `CLAUDE.md` are symlinked into new slots automatically, but MCP config is mixed with the per-account identity in `.claude.json`, so it is shared with this explicit merge. Run it after logging into your slots (a slot has no `.claude.json` until first login). |
| `swapdex ls [--json] [--names]` | List saved profiles with the account email, tier, and a `(expired)` / `(stale)` / `(unreadable)` marker. The active account is marked from the **pointer** a switch sets, falling back to the live login where no slot points anywhere; a login left in the tool's own dir gets its own line rather than being shown as active. `--names` prints bare names one per line (for scripts and completion). |
| `swapdex status [--json] [--short]` | Show the active account per tool, matched back to a saved profile, plus expiry and a session summary (needs sessionwiki). `--json` for scripting; `--short` prints one compact `claude:work codex:personal` line for shell prompts and statuslines. |
| `swapdex restore [--tool ...] [--dry-run]` | Put back the login that was live before the last switch (`use` backs it up first, even when it was never saved as a profile). Backs up the current login before applying, so running it again toggles back. |
| `swapdex rm <name> [--yes]` | Remove a saved profile. Asks y/N on a terminal; `--yes` skips the question (and is required when stdin is not a tty, e.g. scripts). Never touches a live login. |
| `swapdex rename <old> <new>` | Rename a saved profile. |
| `swapdex sessions [--json]` | Sessions grouped by the account active when they ran (best-effort; needs sessionwiki on PATH - the ui's session menu itself does NOT). |
| `swapdex usage [--json]` | Recent local token usage per tool over the last 5h and 7d, summed from `~/.claude` and `~/.codex` session logs - **per account** once a switch history exists (each event is attributed to the profile active at its timestamp; what predates your first switch shows as untagged). A rough activity gauge, not the billed quota. Reads local files only - never the network. |
| `swapdex quota [--json]` | Read Claude and Codex account usage windows from their provider endpoints using the account’s own credential; no model request is submitted. The command and dashboard never invoke OAuth renewal or update saved credentials. Expired credentials are reported without a usage request; a running native Claude session may supply its current token only for the same account. Missing, rejected, throttled or unavailable readings remain explicit instead of becoming a guessed percentage. `--json` includes per-tool results and unexpected response shapes. A provider read failure is reported in the result. |
| `swapdex serve [<name>] [--tool ...] [--off] [--quiet]` | Hand turns to an account without moving where new sessions start. Two pointers on purpose: `serve` decides who PAYS, `use` decides where new conversations LIVE. Bare `serve` shows who is serving; `--off` persistently forwards each client's own authentication, including across proxy restarts and when `--account` was supplied. `use`, `restore`, or `serve <name>` enables managed serving again. With serving off, usage without evidence of the client's account remains unattributed. `--quiet` prints one line for a status bar: the account, its login, and what it has left, from cache. |
| `swapdex refresh [<name>] [--keep-alive]` | Attempt renewal for eligible Claude/Codex accounts, or one named account. `--keep-alive` also sweeps idle accounts approaching access expiry. Coordinated renewal groups matching login identities and rotating-token copies; native sessions keep ownership of their refresh tokens. Renewal can still require a new sign-in after provider expiry or revocation. Manual refresh exits 4 if a requested renewal fails or is deferred, including partial success; successful/current/empty runs return 0. Keep-alive reports scheduled deferrals separately and exits 4 for failures. |
| `swapdex service <install\|uninstall\|status> [--tool ...]` | Keep each tool’s proxy running through launchd/systemd. Install/uninstall default to Claude; pass `--tool codex` for Codex. Scheduled renewal runs inside a running proxy, whether service-managed or foreground; the service keeps it available independently of the launching terminal. |
| `swapdex pause <name>` / `swapdex resume <name>` | Keep an account out of the proxy's automatic rotation, or put it back. `use` and `serve` still reach it by name - this is only about what the proxy picks on its own. `ls` marks the row `(rotation paused)` and `ls --json` carries `"paused"`. |
| `swapdex whereis [<project>]` | Find which account holds a conversation, searching every account's store. Prints the command that resumes it, per account. |
| `swapdex share-history [--tool ...] [--dry-run]` | One-time repair: make every conversation reachable from every account. Links each slot's conversation store to the shared one and carries over anything only that slot had. A slot holding its own store is reported and left alone rather than linked over. |
| `swapdex repair-codex-sessions [--dry-run] [--quiet]` | Recover sessions hidden by legacy Swapdex provider IDs. Repairs metadata and compatible search indexes in known Codex homes with a private recovery journal; keeps conversation contents and paginated history intact. The Codex shim runs this automatically. Busy threads are deferred, and unsupported compressed files or failed repairs remain visible and retryable. |
| `swapdex slash` | Install a `/swap` command for Claude Code (`~/.claude/commands/swap.md`) and a matching skill for Codex, so an account can be switched without leaving the chat. |
| `swapdex export <file>` / `swapdex import <file>` | Write this machine's account setup - names and settings, **never a login** - and re-create it elsewhere. Accounts already present are left alone, and each still needs its own sign-in. |
| `swapdex auto <on\|off>` | Whether the proxy hands a spent session to another account by itself. Read on every request, so it reaches a proxy that is already running. |
| `swapdex strategy <roomiest\|consume-first>` | Which account auto-continue reaches for: the one with the most headroom, or the current one until it is spent. |
| `swapdex threshold <value\|off>` | Configure a usage threshold (`0.9` or `90%`) for leaving an account. A suffix is literal: `0.5%` means half a percent, while `0.5` means 50%. Valid values below 5% are retained. Roomiest still requires a 10 percentage point improvement before moving; ConsumeFirst uses its existing selection rules. Enabled thresholds add provider usage reads. Proxy model forwarding and eligible OAuth renewal are separate network activity. |
| `swapdex fallback-model <model\|off>` | A cheaper model to ask for when no account qualifies for automatic rotation. Off by default, and only used when there is nowhere else to go. |
| `swapdex doctor` | Check store permissions, snapshots, live logins, account homes, native executables, shims and services. It also checks the published version online; a failed online comparison is reported without treating it as a broken installation. Findings include a remedy. Exits 0 when healthy or 9 when problems are found. |
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
| `4` | The required lock is held by another Swapdex operation or cannot be opened, or a requested refresh failed/was deferred (see `refresh`). A failed settings lock leaves the settings file unchanged. |
| `5` | No profile by that name (`use` / `rm` / `rename`), or no backup (`restore`). |
| `6` | The profile already has a snapshot for that tool; pass `--update` (`add`); or the target name already exists (`rename`). |
| `7` | `rm` was called without `--yes`; or `add --update` refused to repoint a profile to a DIFFERENT account (repointing must be explicit). |
| `8` | `login` was started but the tool's login flow did not complete. |
| `9` | `doctor` found at least one problem. |

## Tools

`--tool` accepts `claude` (alias `claude-code`), `codex`, `gemini`,
`antigravity`, or `all` (alias `both`) where that command supports them.
Defaults are command-specific: `add` and `use` cover relevant tools, `login`
asks on a terminal, and `run`, `adopt`, `serve`, `proxy`, and service installation
select Claude when omitted. Use an explicit `--tool codex` in Codex commands.
Slot launches and managed proxy requests support Claude and Codex; Gemini and
Antigravity use saved snapshots. Migration accepts only Claude/Codex and covers
both when omitted. Run `swapdex <command> --help` for its accepted flags.

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
