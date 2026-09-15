<div align="center">

<img src="https://raw.githubusercontent.com/youdie006/swapdex/main/docs/cli-banner.png" alt="swapdex - switch Claude Code, Codex, Gemini CLI and Antigravity login accounts, one command, all local" width="760" />

[![CI](https://github.com/youdie006/swapdex/actions/workflows/ci.yml/badge.svg?branch=main)](https://github.com/youdie006/swapdex/actions/workflows/ci.yml)
[![crates.io](https://img.shields.io/crates/v/swapdex?logo=rust&color=7a3be0)](https://crates.io/crates/swapdex)
[![npm](https://img.shields.io/npm/v/%40youdie006%2Fswapdex?logo=npm&color=7a3be0)](https://www.npmjs.com/package/@youdie006/swapdex)
[![license](https://img.shields.io/badge/license-MIT-1e1d1a.svg)](LICENSE)
[![switcher: no network](https://img.shields.io/badge/switcher-no%20network-7a3be0.svg)](#what-it-will-not-do)

</div>

One command to flip your Claude Code, Codex, Gemini CLI, or Antigravity from
your work account to your personal one, and back. No re-login, no browser, no copying
tokens around -- and the switch itself never touches the network. (One opt-in
command, `swapdex quota`, reads your remaining balance from Anthropic; nothing
else does.)

<div align="center">
<img src="https://raw.githubusercontent.com/youdie006/swapdex/main/docs/demo.gif" alt="swapdex demo: ls lists two saved accounts, use personal switches Claude Code and Codex together, status confirms both, restore puts the previous login back" width="760" />
</div>

```sh
brew install youdie006/tap/swapdex     # macOS / Linux
npm  i -g @youdie006/swapdex           # or, if you have node
cargo install swapdex                  # or, if you have rust
```

Then `swapdex add work`, `swapdex add personal`, and `swapdex use personal`.
[Full install notes](#install) &middot; [what it will not do](#what-it-will-not-do).

---

## Why

If you run Claude Code, Codex, Gemini CLI, or Antigravity under more than one
account -- a work seat and a personal subscription, a client's org and your own
-- switching means logging out and back in every time.

swapdex gives each account its **own permanent space** -- its own
`CLAUDE_CONFIG_DIR` or `CODEX_HOME` slot -- and switches the default pointer.
`swapdex use work` points your default account there and a plain `claude`
follows it; `swapdex run work` launches straight into that account (each terminal
can be a different one). Existing native sessions keep their own slot when the
default changes.
`swapdex onboard` sets this up in a few prompts.

It manages accounts you already own, with separate launch defaults, proxy
selection and configurable failover. See [How it works](#how-it-works) for the
difference between permanent slots and legacy saved snapshots.

Each account signs in within its own slot. The optional local proxy uses the
selected account's credential for requests and can renew idle slots in place.
When an actual native session owns the same login, Swapdex leaves renewal to
that application and uses a verified, read-only access snapshot when available.

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
Claude and Codex profiles whose accounts are not already slotted onto their own
slots.</sub>

## Install

```sh
# npm - you already have it, since Claude Code and Codex ship this way
npm install -g @youdie006/swapdex

# Homebrew (macOS / Linux)
brew install youdie006/tap/swapdex

# crates.io (needs a Rust toolchain)
cargo install swapdex

# or the one-liner (prebuilt binary -> ~/.local/bin)
curl -fsSL https://raw.githubusercontent.com/youdie006/swapdex/main/install.sh | sh
```

Pick one and stay with it. Each installer wants the same name on `PATH`, and
with two of them the shims keep calling whichever copy wrote them - so updating
the other one changes nothing, silently. `swapdex doctor` reports this, along
with whether the version you are running is the one that is published.

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

# Set up a second machine with the same accounts (never carries a login)
swapdex export setup.json    # on the machine you already use
swapdex import setup.json    # on the new one, then sign each account in

# Anything off? Every finding comes with its fix
swapdex doctor
```

The classic snapshot commands still work for the shared-slot workflow: `swapdex
add <name>` snapshots the current login, `swapdex use <name>` swaps it back
(backed up first, and refused while a `claude` session is running on that login
so it can't be logged out), `swapdex restore` undoes the last swap, and `swapdex
ui` is the full-screen picker. `swapdex migrate` matches Claude and Codex
profiles to slots by account and creates spaces only for accounts without one.

`status` shows the active account per tool, matched back to a saved profile:

```
claude-code: you@work.com [max] (profile 'work')
codex: you@personal.com [chatgpt] (profile 'personal')
```

The active account is read from the **pointer** a switch sets, and falls back
to the live login on a machine that has no slots. Where the tool's own config
dir holds a different account -- you signed in directly without the shim, or an
old copy-model switch left one behind -- that gets its own line rather than
being shown as the active account:

```
codex: you@work.com (profile 'work')
  (a plain `codex` would launch on 'personal' instead - `swapdex shim`
   makes it follow your switches)
```

Both are true and they answer different questions, so swapdex prints both
instead of picking one. A machine sat in exactly that state for six days.

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
message quota. It uses the slot or a verified current native login for that
account. An unavailable or expired credential reports its state rather than
inventing current quota. It is also in `swapdex ui` under the `%` key.

### The dashboard

`swapdex ui` is the same thing without the commands: your accounts, which one is
active, and how much each has left. On a machine with no profiles yet it opens on
what you are *already* signed into and offers to save that as your first one, so
setup is one keystroke and a name.

<div align="center">
<img src="https://raw.githubusercontent.com/youdie006/swapdex/main/docs/ui-demo.gif" alt="swapdex ui on a fresh machine: it finds the Claude Code and Codex logins already present, saves them as a profile named main, and shows the account with its 5h and 7d usage bars" width="760" />
</div>

## Resuming Codex conversations

Use `codex resume` normally, or `codex resume --all` to include other working
directories. Swapdex keeps one stable OpenAI provider across account changes.
The paying account is shown by `swapdex serve --tool codex --quiet`.
If the proxy cannot start, the launcher warns that Codex will use its own login
directly.

After updating from a version that created `swapdex` provider IDs, run
`swapdex shim` to refresh the launcher. It automatically repairs those legacy
session labels before launching Codex. To inspect or retry the repair directly:

```sh
swapdex repair-codex-sessions --dry-run
swapdex repair-codex-sessions
```

The repair preserves conversation contents and keeps a private recovery journal.
Open sessions are deferred until they close. Unsupported compressed rollouts
and unsuccessful repairs are reported rather than silently hidden.

## Keeping accounts from expiring

Access tokens expire by design. An idle slot can renew while its refresh token
remains valid, but provider expiry, revocation or renewal by another credential
holder can make a new browser sign-in necessary. Keep-alive reduces avoidable
idle expiry; it cannot guarantee that a login never expires.

swapdex renews idle accounts for you, but **only while its proxy is running**,
because that is the process holding the timer:

```sh
swapdex service install --tool claude
swapdex service install --tool codex
```

That installs a launchd/systemd unit per tool. The proxy then sweeps every
thirty minutes and renews anything approaching its deadline, including slots
nobody has opened. A slot the tool is running in is never touched: its own
session holds the refresh token, and renewing from outside would retire the one
that session is about to use.

Without the service, nothing is on a timer. You can sweep by hand:

```sh
swapdex refresh --keep-alive     # renew every account heading for expiry
swapdex refresh <name>           # renew one that has already lapsed
```

`swapdex doctor` reports whether the service is installed and running.

## How it works

**Slots (the model swapdex uses now).** Each account gets its own
`CLAUDE_CONFIG_DIR` -- a directory under `~/.local/share/swapdex/slots/`, or a
`~/.claude-*` dir you adopt. Claude keys its login to that dir (a file on Linux,
a Keychain item on macOS), so each account's token lives and refreshes *in its
own slot*. swapdex never copies a token between slots: `swapdex run <name>`
`exec`s `claude` with that slot's `CLAUDE_CONFIG_DIR`, and `swapdex use <name>`
writes a one-line pointer that a small `claude` shim on your PATH reads. Shared
config (`settings.json`, global `CLAUDE.md`) is symlinked into each new slot;
the token and history stay per-slot. Independently signed-in slots avoid sharing
a rotating refresh chain. Copies of one login remain coupled even if they live
in different directories; the warning below applies to those copies.

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
migrate [--tool claude|codex]` moves unslotted Claude and Codex profiles onto
their own slots, retiring the shared homes.

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

**Do not copy a credential out of the store for something else to use.** These
refresh tokens are single-use: the server retires the outgoing one whenever a
holder renews, so two programs holding one account's credential silently
retire each other's. The copy that missed a renewal keeps working until its
access token lapses, which is why the failure arrives hours or days after the
change that caused it -- one such split cost a scheduled job 42 hours. A
program that needs its own Codex or Claude access should sign in for itself;
`swapdex` is for accounts a person switches between, not a credential source
for other software.

Codex renewal is checked before access expires: a running Codex proxy checks
every 30 minutes and attempts renewal for idle slots within 48 hours of expiry.
`swapdex refresh [name]` exits with status 4 if any requested renewal fails or
is deferred by the running-session guard, even if another account renews.
Already-current accounts and successful or empty runs return 0. Missing or
unreadable logins produce a sign-in remedy without an OAuth request.

`swapdex refresh --keep-alive` runs the same check without a proxy. If a local
session holds a due account, Swapdex leaves its refresh token alone. A verified
usable native login is shown as managed by Claude or Codex, including
`renewal_owner` in `ls --json`; this is not an OAuth renewal by Swapdex. When
that native login cannot be verified, renewal remains deferred and unverified.
Actual access expiry and recorded refresh rejection are separate warnings.
An inaccessible macOS Keychain is a read-access problem, not proof that the
login expired.

For an HTTP request rejected with 401, the managed proxy first attempts bounded
recovery of the same selected account: reread a changed usable native access
token, or await a coordinated Swapdex renewal for an idle login. It retries
only with a changed usable token, before any explicitly configured failover.
An unavailable selected login produces an error instead of silently using the
client's different account.

The launch default and proxy selection control different operations: the first
affects new native launches, and the second affects subsequent managed HTTP
requests. Changing a file or default does not switch every existing native
process, in-flight request or WebSocket conversation. Explicit account pins
retain their selected account.

An external copy can renew without changing any local file. Neither the local
access token's issue time nor `last_refresh` reveals that remote event. These
checks therefore cannot certify refresh validity after unseen remote activity;
external consumers need their own login instead of a copy of a managed slot.

### Network and credential behavior

Account selection and listing read local state. Opt-in quota lookups contact
provider usage endpoints. The optional proxy relays API requests with the
selected credential, and its scheduled renewal work contacts OAuth endpoints
for idle logins. It uses `ureq` with rustls and bundled roots; CI excludes heavy
async runtimes and system-TLS dependencies.

Explicit account selection, launch defaults and configured proxy failover are
separate controls. A local file lock coordinates participating Swapdex callers;
it cannot lock an independent native CLI or another machine. Native renewal
ownership and access availability are therefore reported separately.

There is no command that prints a saved credential. OAuth request secrets are
passed to curl on stdin, and diagnostics redact credentials. Native launches
execute the installed official CLI with the chosen account's configuration.

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
  This older comparison should be read alongside the current source review below.
- [aisw](https://github.com/burakdede/aisw) -- cross-tool including Gemini,
  OS-keyring storage, Windows support. More features, bigger surface.
- [caam](https://github.com/Dicklesworthstone/coding_agent_account_manager) --
  cross-tool with a shell wrapper and automatic rotation on rate limits.

For current Codex switching and renewal mechanisms, see the
[2026-09-15 source survey](docs/research/2026-09-15-codex-switcher-survey.md):
pinned implementations, regression-test coverage and the limits of file-based
switching while native sessions keep running.

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
