<div align="center">

<img src="https://raw.githubusercontent.com/youdie006/swapdex/main/docs/cli-banner.png" alt="swapdex - switch Claude Code and Codex login accounts, one command, all local" width="760" />

[![CI](https://github.com/youdie006/swapdex/actions/workflows/ci.yml/badge.svg?branch=main)](https://github.com/youdie006/swapdex/actions/workflows/ci.yml)
[![license](https://img.shields.io/badge/license-MIT-1e1d1a.svg)](LICENSE)
[![switcher: no network](https://img.shields.io/badge/switcher-no%20network-7a3be0.svg)](#what-it-will-not-do)

</div>

One command to flip your Claude Code, Codex, Gemini CLI, or Antigravity from
your work account to your personal one, and back. No re-login, no browser, no copying
tokens around -- and the switch itself never touches the network. (One opt-in
command, `swapdex quota`, reads your remaining balance from Anthropic; nothing
else does.)

<div align="center">
<img src="https://raw.githubusercontent.com/youdie006/swapdex/main/docs/demo.gif" alt="swapdex demo: ls, use personal, status, restore, doctor" width="760" />
</div>

---

## Why

If you run Claude Code, Codex, Gemini CLI, or Antigravity under more than one
account -- a work seat and a personal subscription, a client's org and your own
-- switching means logging out and back in every time.

swapdex gives each account its **own permanent space** -- its own
`CLAUDE_CONFIG_DIR` slot -- and flips between them without ever copying a token.
`swapdex use work` points your default account there and a plain `claude`
follows it; `swapdex run work` launches straight into that account (each terminal
can be a different one). Because nothing is copied, **a switch can never log an
account out** -- even if a session is still running when you switch.
`swapdex onboard` sets this up in a few prompts.

It is a **switcher, not a rotator.** It manages accounts you already own for
distinct purposes, with no feature for cycling them to get around a rate limit
-- see [What it will not do](#what-it-will-not-do).

Safety is the design center: in the slot model swapdex never writes a credential
at all -- each account's own login creates and refreshes its token, in its own
slot -- and it only ever hands the official CLI its own credentials: no wrapper,
no proxy, no client spoofing.

## Concepts

- **Account** -- one login you own (a work seat, a personal subscription). Its
  redacted identity (email, tier) is shown by `slots`, `status`, and `doctor`;
  never a token.
- **Slot** -- an account's own permanent `CLAUDE_CONFIG_DIR`, where its login
  lives and refreshes in place. swapdex creates one per account (or adopts a
  `~/.claude-*` dir you already use) and never copies tokens between them.
- **Default account** -- the one a plain `claude` uses, via a tiny shim on your
  PATH. `swapdex use <name>` repoints it; `swapdex run <name>` ignores it and
  launches a specific account directly.

<sub>swapdex still keeps the classic snapshot commands (`add` copies a live login
into a profile, `use` on that profile swaps it back, guarded against the
running-session logout) for the shared-slot workflow; `swapdex migrate` moves
those profiles onto their own slots.</sub>

## Install

```sh
# crates.io (Rust)
cargo install swapdex

# Homebrew (macOS / Linux)
brew install youdie006/tap/swapdex

# npm (downloads the prebuilt binary)
npm install -g @youdie006/swapdex

# or the one-liner (prebuilt binary -> ~/.local/bin)
curl -fsSL https://raw.githubusercontent.com/youdie006/swapdex/main/install.sh | sh
```

Linux, WSL, and macOS (Claude's macOS login lives in the Keychain; swapdex
swaps it there, via `/usr/bin/security`). Requires at least one supported CLI
(Claude Code, Codex, Gemini, Antigravity) already installed and logged in. Full command, exit-code, and environment
reference: [docs/COMMANDS.md](docs/COMMANDS.md).

## Use

```sh
# First run: guided setup -- registers ~/.claude-* dirs you already use,
# moves old profiles onto slots, offers the shim. A bare `swapdex` runs this
# automatically the first time there is something to set up.
swapdex onboard

# Launch an account in its own slot (first time = sign in; concurrent-safe,
# so each terminal can be a different account)
swapdex run work
swapdex run personal

# Make a plain `claude` follow a default account
swapdex shim                # installs the claude shim once (prints a PATH line)
swapdex use personal        # a plain `claude` now runs as personal
swapdex use work            # switch the default -- no re-login, never logs out

# See your accounts and who's active
swapdex slots
swapdex status

# Register a config dir you already run by hand; move old profiles to slots
swapdex adopt company ~/.claude-company
swapdex migrate

# Sessions grouped by the account active when they ran (needs sessionwiki)
swapdex sessions

# Recent local token usage per tool (5h/7d) -- tells you when to switch
swapdex usage

# Remaining quota per Claude account -- the one opt-in network read
swapdex quota

# Anything off? Every finding comes with its fix
swapdex doctor
```

The classic snapshot commands still work for the shared-slot workflow: `swapdex
add <name>` snapshots the current login, `swapdex use <name>` swaps it back
(backed up first, and refused while a `claude` session is running on that login
so it can't be logged out), `swapdex restore` undoes the last swap, and `swapdex
ui` is the full-screen picker. `swapdex migrate` moves these onto their own slots.

`status` shows the live account per tool, matched back to a saved profile:

```
claude-code: you@work.com [max] (profile 'work')
codex: you@personal.com [chatgpt] (profile 'personal')
```

The active account is always read from the **live** login, so if you `/login`
directly in the CLI, swapdex reports the truth rather than a stale guess.

For your shell prompt or statusline, `status --short` prints one compact line:

```sh
$ swapdex status --short
claude:work codex:personal
```

e.g. in a starship prompt: `command = "swapdex status --short"` in a
[custom module](https://starship.rs/config/#custom-commands), or in `PS1`
via `$(swapdex status --short)`.

It also drops straight into **Claude Code's own status line**, so the active
account is always visible inside the tool you are switching
(`~/.claude/settings.json`):

```json
{
  "statusLine": { "type": "command", "command": "swapdex status --short" }
}
```

`usage` reads your local session logs (no network) to gauge how heavily you've
been using each tool lately, so you know when to switch to a fresher account:

```
Local usage - this machine, approximate (not the billed quota):
  claude-code  5h:   8.2M tok / 12 sess    7d:   61.4M tok / 88 sess
    @work        5h:   6.0M tok           7d:    40.1M tok
    @personal    5h:   2.2M tok           7d:    19.3M tok
```

Once a switch history exists, tokens are attributed to the profile active at
each event's timestamp (the same honest join `sessions` uses); anything before
your first switch stays untagged. Still deliberately a hint, not a
quota-dodging auto-rotator.

Where `usage` is your local activity, `quota` is the vendor's actual remaining
balance -- the one command that reaches the network, and only when you run it:

```
$ swapdex quota
quota - remaining on your Claude accounts
live from Anthropic's usage endpoint; opt-in network, spends 0 message quota.

work (active)   you@work.com
  5h        ▓▓▓▓▓▓▓░░░   68% left   resets in 2h 14m
  7d        ▓▓▓▓▓▓░░░░   57% left   resets in 3d 4h

personal   you@personal.com
  snapshot token expired - `swapdex use personal` to refresh, then `swapdex quota`
```

It reads each account's remaining quota from Anthropic's official OAuth usage
endpoint using that account's **own** token -- read-only, and it spends zero
message quota. The active account is always live; a saved account whose token
has expired reports so rather than showing a stale number (swapdex never
refreshes tokens -- that is the line between a switcher and a rotator). It is
also in `swapdex ui` under the `%` key.

## How it works

**Slots (the model swapdex uses now).** Each account gets its own
`CLAUDE_CONFIG_DIR` -- a directory under `~/.local/share/swapdex/slots/`, or a
`~/.claude-*` dir you adopt. Claude keys its login to that dir (a file on Linux,
a Keychain item on macOS), so each account's token lives and refreshes *in its
own slot*. swapdex never copies a token between slots: `swapdex run <name>`
`exec`s `claude` with that slot's `CLAUDE_CONFIG_DIR`, and `swapdex use <name>`
writes a one-line pointer that a small `claude` shim on your PATH reads. Shared
config (`settings.json`, global `CLAUDE.md`) is symlinked into each new slot;
the token and history stay per-slot. Because no credential is ever moved, a
token refresh in one account can never revoke another -- **a switch cannot log
you out**.

**Classic snapshots (still supported).** Each CLI also keeps its login in a
small on-disk file:

- Claude Code: `~/.claude/.credentials.json` plus the `oauthAccount` block inside
  `~/.claude.json`
- Codex: `~/.codex/auth.json`
- Gemini CLI: `~/.gemini/oauth_creds.json` plus `~/.gemini/google_accounts.json`
- Antigravity: `~/.gemini/antigravity-cli/antigravity-oauth-token`

`add` copies the current login into a private store at `~/.local/share/swapdex`;
`use` on a snapshot profile writes it back atomically, backing up the current
login first, and only the `oauthAccount` block of `~/.claude.json` is swapped so
your projects, MCP servers, and settings are untouched. That switch is refused
while a `claude` session is running on the same login slot, since the session's
next token refresh would otherwise revoke the saved copy. On macOS the Claude
token lives in the login Keychain, one item per `CLAUDE_CONFIG_DIR`. `swapdex
migrate` moves these profiles onto their own slots, retiring the shared slot.

## Safety

- Every credential file swapdex writes is `0600`; the store directory is `0700`.
- Writes are atomic (temp file created `0600`, then renamed) so an interrupted
  switch can never leave a half-written credential that bricks the CLI.
- Symlinked credential paths and running as root are refused.
- `use` writes a backup of the current login (fsynced, or the switch aborts;
  exception: an unreadable/corrupt live file is skipped with a warning - `use`
  is exactly the command that can replace a corrupt login)
  before overwriting anything, and `swapdex restore` brings it back in one
  command if the switch was a mistake. The store keeps the last 2 backups per
  tool, and `use` warns when the outgoing login is not saved as a profile --
  so save accounts you care about with `add`.
- No token, refresh token, or home path is ever printed.

**The store holds plaintext refresh tokens.** Protect `~/.local/share/swapdex`
like `~/.ssh`, and do not sync it across machines (it is single-machine,
single-user by design).

### What it will not do

These are structural properties, not promises -- the code is built so they
cannot happen:

- **No HTTP client, no background network.** The binary has no HTTP client in
  its dependency graph (CI asserts this on every commit), so it cannot phone
  home or exfiltrate a token. Switching, `ls`, `status`, `usage` -- all 100%
  local. The one exception is the opt-in `swapdex quota` command, which shells
  out to `curl` to read your *own* remaining balance from Anthropic's official
  usage endpoint (that account's own token, read-only, spends zero message
  quota). It runs only when you type it, sends no data anywhere, and touches no
  other endpoint.
- **No auto-rotation.** There is no `--auto`, `--next`, or
  `--when-rate-limited` flag. `use` only ever switches to a name you type.
- **No token export.** There is no command that prints a saved credential.
- **No wrapper, no client spoofing.** swapdex swaps the credential file that the
  official `claude` / `codex` binary already reads, then gets out of the way. It
  never sits between the CLI and the API, never proxies requests, and never
  presents itself as the official client. Your traffic is the real CLI's traffic.
  (Launching the official tool once, on your explicit pick - `login`'s sign-in
  flow, `ui`'s session resume - is a hand-off, not a wrapper: swapdex `exec`s
  and is gone.)

Anthropic and OpenAI both permit multiple accounts for genuinely different
purposes but forbid using multiple accounts to get around a single workload's
rate limit, and forbid routing subscription OAuth tokens through third-party
tools or spoofing the official client. swapdex is built for the former and
structurally cannot do the latter -- it only ever hands the real CLI its own
credentials. See
[Anthropic Usage Policy](https://www.anthropic.com/legal/usage-policy) and
[OpenAI Usage Policies](https://openai.com/policies/usage-policies/).

## MCP (read-only)

`swapdex mcp` runs a read-only MCP server exposing `whoami` and `list_accounts`
so an agent can see which account is active. There is deliberately **no** switch
tool -- an agent can never change your account.

```sh
claude mcp add swapdex -s user -- swapdex mcp
```

## Works with

swapdex is the accounts layer of a small local AI-CLI stack:

- [sessionwiki](https://github.com/youdie006/sessionwiki) -- index, search, and
  resume your AI coding sessions. `swapdex sessions` groups them by account,
  and after a switch in `swapdex ui` you get that account's recent sessions
  with a `sessionwiki resume <id>` hint -- switch, land back in your work.
- [prodex](https://github.com/youdie006/prodex) -- share one logged-in ChatGPT
  Pro session across agents. swapdex coexists with it without touching its auth.

## Alternatives

Good tools exist in this space; they make different trade-offs (each line from
that project's README, July 2026):

- [claude-swap](https://github.com/realiti4/claude-swap) -- Claude Code only,
  a TUI with live usage bars, and *optional auto-switching* near your limit.
  If you want auto-rotation, use it -- swapdex deliberately refuses to have
  that feature.
- [aisw](https://github.com/burakdede/aisw) -- cross-tool including Gemini,
  OS-keyring storage, Windows support. More features, bigger surface.
- [caam](https://github.com/Dicklesworthstone/coding_agent_account_manager) --
  cross-tool with a shell wrapper and automatic rotation on rate limits; the
  philosophical opposite of swapdex.

Pick swapdex if you want the smallest thing that switches your AI CLIs
together, can always undo (`restore`), diagnoses itself (`doctor`), shows
your remaining balance (`quota`), and structurally cannot rotate, proxy, or
spoof the official client.

## Roadmap

- ~~Claude Code on macOS (Keychain).~~ **Shipped** (0.17-0.24): swapdex swaps
  Claude's login inside the macOS Keychain via `/usr/bin/security`, resolves
  the item exactly the way `claude` itself does (one item per
  `CLAUDE_CONFIG_DIR` profile), and `doctor` diagnoses any mismatch.
- ~~Permanent per-account slots.~~ **Shipped** (0.26): each account gets its own
  `CLAUDE_CONFIG_DIR`, so a switch copies no token and can never log an account
  out -- even with a session running. `run`, `use` (repoint) + the `claude`
  shim, `onboard`, `adopt`, `migrate`, and `sync-mcp` (shares your MCP servers
  across slots, since they live in the per-account `.claude.json`).

Being considered, explicitly opt-in and advisory-only:

- **Per-directory hints (cross-tool).** Bind a directory to a profile and have
  `swapdex resolve <dir>` *suggest* the right account ("this directory is bound
  to `work` -- run `swapdex use work`"). It would cover both Claude
  (`CLAUDE_CONFIG_DIR`) and Codex (`CODEX_HOME`) in one binding. It will never be
  a shell wrapper, never auto-switch, and never let anything but an explicit
  `swapdex use` change the active account -- that bright line is what keeps
  swapdex a switcher, not a rotator.

## License

MIT
