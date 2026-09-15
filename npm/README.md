<div align="center">

<img src="https://raw.githubusercontent.com/youdie006/swapdex/main/docs/cli-banner.png" alt="swapdex - switch Claude Code, Codex, Gemini CLI and Antigravity login accounts, one command, all local" width="760" />

[![CI](https://github.com/youdie006/swapdex/actions/workflows/ci.yml/badge.svg?branch=main)](https://github.com/youdie006/swapdex/actions/workflows/ci.yml)
[![crates.io](https://img.shields.io/crates/v/swapdex?logo=rust&color=7a3be0)](https://crates.io/crates/swapdex)
[![npm](https://img.shields.io/npm/v/%40youdie006%2Fswapdex?logo=npm&color=7a3be0)](https://www.npmjs.com/package/@youdie006/swapdex)
[![license](https://img.shields.io/badge/license-MIT-1e1d1a.svg)](LICENSE)
[![selection: local](https://img.shields.io/badge/selection-local-7a3be0.svg)](#network-and-credential-behavior)

</div>

Keep your Claude Code and Codex accounts in separate login directories, then
choose which account handles the next request in a managed conversation.
Sign each account in once; subsequent account selections do not require a new
login while its credentials remain usable. Gemini CLI and Antigravity use the
supported snapshot-switching workflow.

```sh
brew install youdie006/tap/swapdex     # macOS / Linux
npm  i -g @youdie006/swapdex           # or, if you have node
cargo install swapdex                  # or, if you have rust
```

Start with the [Claude/Codex quickstart](#quick-start).
[Install notes](#install) &middot; [existing logins](#existing-logins-and-folders)
&middot; [network behavior](#network-and-credential-behavior).

---

## Why

If you run Claude Code, Codex, Gemini CLI, or Antigravity under more than one
account -- a work seat and a personal subscription, a client's org and your own
-- switching means logging out and back in every time.

swapdex gives each Claude or Codex account its **own permanent space** -- its
own `CLAUDE_CONFIG_DIR` or `CODEX_HOME` slot. A small launcher called a **shim**
makes plain `claude` or `codex` commands use the selected space and local proxy.
The proxy chooses an account for each managed request, so changing the serving
account keeps the conversation and its working directory in place.

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
- **Slot** -- an account's permanent Claude or Codex directory, where its login
  lives and refreshes in place. Swapdex creates it, or registers a separate
  directory you already use. Each new slot needs its own native sign-in.
- **Launch default** -- the slot a plain `claude` or `codex` starts in through
  the shim. `swapdex use <name> --tool codex` selects a Codex default.
- **Serving account** -- the account the proxy uses for subsequent managed
  requests. `swapdex serve <name> --tool codex` changes it without moving the
  conversation's home. `swapdex run` launches the named slot directly, so it
  is useful for login and for sessions that should use their own account.

<sub>swapdex still keeps the classic snapshot commands (`add` copies a live login
into a profile, `use` on that profile swaps it back, guarded against the
running-session logout) for the shared-slot workflow; `swapdex migrate` moves
Claude and Codex profiles whose accounts are not already slotted onto their own
slots.</sub>

## Install

```sh
# npm (requires Node.js and npm)
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

Linux, WSL, and macOS are supported. Install the native CLI you want to use
first; Swapdex does not install it. Codex-only and Claude-only machines are both
supported. Claude's macOS login uses the Keychain through `/usr/bin/security`.
For WSL, install and run Swapdex and the native CLI inside the same WSL
distribution. [Full command reference](docs/COMMANDS.md).

## Quick start

Choose the tool you have. `work` and `personal` are example names; sign into
the intended account in each login flow. The second account is optional.

### Codex

```sh
swapdex run work --tool codex -- login --device-auth
swapdex run personal --tool codex -- login --device-auth
swapdex shim
```

Activate the PATH change printed by `swapdex shim`: open a new terminal, source
the shell file it names, or apply its printed `export PATH=...` command. Then:

```sh
swapdex use work --tool codex
codex
```

While that conversation stays open, use another terminal to select the account
for its next request:

```sh
swapdex serve personal --tool codex
swapdex serve --tool codex --quiet
```

The last command shows the selected serving account. It is a routing status,
not an independent billing statement. A request already in progress finishes
with the account it started with.

### Claude Code

```sh
swapdex run work --tool claude -- auth login
swapdex run personal --tool claude -- auth login
swapdex shim
```

Activate the printed PATH change, then start a managed conversation:

```sh
swapdex use work --tool claude
claude
```

In another terminal, `swapdex serve personal --tool claude` selects the account
for the next managed request. `swapdex serve --tool claude --quiet` shows it.

**Existing conversations:** a native process started before the shim was
installed, or started directly with `swapdex run`, keeps its direct routing.
Resume it once through plain `codex resume` or `claude --resume` after activating
the shim. Subsequent serving-account changes apply without restarting that
managed session. Explicit custom-provider options can also bypass managed
routing. If proxy startup fails, the managed launcher stops with an error.

`swapdex slash` installs an in-chat `/swap` command. `swapdex ui` offers the
account picker and conversation menu. Run `swapdex doctor` if the plain CLI
still uses a different executable or account than expected.

## Existing logins and folders

Already signed in through a default native directory? The quickstart creates
separate slots and leaves that login in place. It requires one sign-in in each
new slot; it does not import the existing credential into those slots.

If you already keep accounts in separate directories, register them in place:

```sh
swapdex adopt work ~/.codex-work --tool codex
swapdex adopt work ~/.claude-work --tool claude
swapdex onboard
```

Run only the `adopt` command for a directory you actually have. `onboard` can
discover `~/.claude-*` directories, offer migration of saved Claude/Codex
profiles, and install available shims. Migration creates missing slots; each
new slot still needs its native sign-in.

### Saved snapshots and other tools

`swapdex setup` guides saving current logins as profiles and adding more.
`swapdex add work --tool codex` saves the Codex login that is already present;
calling `add` again under another name does not sign into a different account.
`swapdex login personal --tool codex` runs the legacy add-another-login flow,
preserving the old login and restoring it if sign-in fails.

`swapdex use <name>` applies a saved snapshot when no matching slot exists,
with backups and running-session guards. `swapdex restore` restores the last
snapshot switch. Gemini and Antigravity use this workflow; the live proxy and
slot quickstart above support Claude and Codex. Snapshot switching does not
reconfigure an already running native process.

## Everyday commands

| Task | Command |
| --- | --- |
| List accounts and their state | `swapdex ls` |
| Show launch defaults | `swapdex status` |
| Show the Codex serving account | `swapdex serve --tool codex --quiet` |
| Find a conversation by project | `swapdex whereis <project>` |
| Group indexed sessions by account | `swapdex sessions` (needs sessionwiki) |
| Read local session activity | `swapdex usage` |
| Fetch Claude/Codex account quota | `swapdex quota` |
| Check paths, accounts and services | `swapdex doctor` |
| Transfer setup without credentials | `swapdex export setup.json`, then `swapdex import setup.json` on the other machine and sign in there |

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

Where `usage` is local activity, `quota` fetches the provider's reported usage
windows for Claude and Codex accounts:

```
$ swapdex quota
quota - remaining on your Claude accounts
live from Anthropic's usage endpoint; opt-in network, spends 0 message quota.

work (active)   you@work.com
  5h        ▓▓▓▓▓▓▓░░░   68% left   resets in 2h 14m
  7d        ▓▓▓▓▓▓░░░░   57% left   resets in 3d 4h

personal   you@personal.com
  usage endpoint rejected this credential - check `swapdex doctor`
```

It reads usage endpoints using each account's **own** token and does not submit
a model request. It uses the slot or a verified current native login for that
account. An unavailable credential or failed lookup reports its state rather
than inventing current quota. The dashboard also fetches quota for its account
rows; `%` opens the detailed panel.

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
If the proxy cannot start, the managed launcher exits with an error before
starting Codex. It does not send the request through a different native login.

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

Swapdex can renew idle accounts **while its proxy is running**,
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

The service keeps that timer available after the launching terminal closes.
A foreground or automatically started proxy also sweeps while it remains
running. You can run a sweep by hand:

```sh
swapdex refresh --keep-alive     # renew every account heading for expiry
swapdex refresh <name>           # renew one that has already lapsed
```

`swapdex doctor` reports whether the service is installed and running.

## How it works

**Slots.** Each Claude or Codex account gets its own `CLAUDE_CONFIG_DIR` or
`CODEX_HOME`, under Swapdex's data directory or in a directory you adopt.
Claude keys its login to that directory (a file on Linux, a Keychain item on
macOS); Codex stores its own auth file there. Each token refreshes in its own
slot. `swapdex run` invokes the native CLI directly with the named slot's home.
`swapdex use` selects the home for plain shimmed launches. Shared configuration
and conversation stores are linked where supported, so selecting a different
serving account does not require copying conversations. Independently signed-in
slots avoid sharing a rotating refresh chain. Copies of one login remain
coupled even when stored in different directories.

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
migrate [--tool claude|codex]` creates missing slots for saved Claude and Codex
accounts. It does not copy their credentials; sign in to each new slot once.

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
- Diagnostics do not print tokens or refresh tokens. Setup commands may show
  the local paths that need to be configured.

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

Account selection and listing use local state. Managed launches and `serve`
can start a local proxy; its scheduled renewal work contacts OAuth endpoints
for idle logins. `quota` and the dashboard contact provider usage endpoints,
and `doctor` checks the published version online. Login commands invoke the
native tool's sign-in flow. The proxy relays model requests with the selected
credential using `ureq`, rustls and bundled roots; CI excludes heavy async
runtimes and system-TLS dependencies. An ordinary account selection does not
submit a model request.

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
