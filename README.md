<div align="center">

<img src="docs/banner.png" alt="swapdex - switch Claude Code and Codex accounts, one command, all local" width="820" />

[![CI](https://github.com/youdie006/swapdex/actions/workflows/ci.yml/badge.svg?branch=main)](https://github.com/youdie006/swapdex/actions/workflows/ci.yml)
[![license](https://img.shields.io/badge/license-MIT-1e1d1a.svg)](LICENSE)
[![local only](https://img.shields.io/badge/network-none-7a3be0.svg)](#what-it-will-not-do)

</div>

One command to flip your Claude Code or Codex CLI from your work account to your
personal one, and back. No re-login, no browser, no copying tokens around.
100% local. Never touches the network.

---

## Why

If you run Claude Code or Codex under more than one account -- a work seat and a
personal subscription, a client's org and your own -- switching means logging
out and back in every time. swapdex snapshots each logged-in account once, then
swaps between them in place: the running CLI picks up the new account on your
next message.

It is a **switcher, not a rotator.** It manages accounts you already own for
distinct purposes. It has no feature for cycling accounts to get around a rate
limit -- see [What it will not do](#what-it-will-not-do).

## Install

```sh
cargo install swapdex
```

Linux / WSL first (macOS Keychain support is planned). Requires the Claude Code
and/or Codex CLI already installed and logged in.

## Use

```sh
# Save the account you're currently logged in as
swapdex add work            # snapshots Claude + Codex, whichever is logged in
swapdex add personal --tool claude

# See what you have and who's active
swapdex ls
swapdex status

# Switch (takes effect on your next message -- no restart)
swapdex use personal
swapdex use work --tool codex
swapdex use work --dry-run          # show what would change, write nothing

# Sessions grouped by the account active when they ran (needs sessionwiki)
swapdex sessions
```

`status` shows the live account per tool, matched back to a saved profile:

```
claude-code: you@work.com [max] (profile 'work')
codex: you@personal.com (profile 'personal')
```

The active account is always read from the **live** login, so if you `/login`
directly in the CLI, swapdex reports the truth rather than a stale guess.

## How it works

Each CLI keeps its login in a small on-disk file:

- Claude Code: `~/.claude/.credentials.json` plus the `oauthAccount` block inside
  `~/.claude.json`
- Codex: `~/.codex/auth.json`

`add` copies the current login into a private store at
`~/.local/share/swapdex`. `use` writes a saved snapshot back into place
atomically, backing up the current login first. For Claude, only the
`oauthAccount` block of `~/.claude.json` is swapped -- your projects, MCP
servers, and settings in that file are never touched.

## Safety

- Every credential file swapdex writes is `0600`; the store directory is `0700`.
- Writes are atomic (temp file created `0600`, then renamed) so an interrupted
  switch can never leave a half-written credential that bricks the CLI.
- Symlinked credential paths and running as root are refused.
- `use` backs up the current login and verifies the backup before overwriting,
  so a switch can never lose an un-saved login.
- No token, refresh token, or home path is ever printed.

**The store holds plaintext refresh tokens.** Protect `~/.local/share/swapdex`
like `~/.ssh`, and do not sync it across machines (it is single-machine,
single-user by design).

### What it will not do

These are structural properties, not promises -- the code is built so they
cannot happen:

- **No network, ever.** The switching binary has no HTTP client in its
  dependency graph (CI asserts this on every commit). swapdex cannot phone home
  or exfiltrate a token.
- **No auto-rotation.** There is no `--auto`, `--next`, or
  `--when-rate-limited` flag. `use` only ever switches to a name you type.
- **No token export.** There is no command that prints a saved credential.

Anthropic and OpenAI both permit multiple accounts for genuinely different
purposes but forbid using multiple accounts to get around a single workload's
rate limit, and forbid using OAuth tokens outside the official CLI. swapdex is
built for the former and structurally cannot do the latter. See
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
  resume your AI coding sessions. `swapdex sessions` groups them by account.
- [prodex](https://github.com/youdie006/prodex) -- share one logged-in ChatGPT
  Pro session across agents. swapdex coexists with it without touching its auth.

## License

MIT
