# Security

swapdex reads and writes the OAuth credential files that Claude Code and Codex
use to authenticate. It is a local, single-user tool. This document states its
trust model and how to report an issue.

## Trust model

- **Local only.** swapdex makes no network calls. The switching binary has no
  HTTP client in its dependency graph, and CI asserts this on every commit.
- **Your accounts only.** It manages the logins already present on your machine.
  It never creates, shares, or transmits credentials.
- **The store holds plaintext refresh tokens.** `~/.local/share/swapdex` is as
  sensitive as the live login, times the number of saved accounts. Protect it
  like `~/.ssh`. Do not sync it across machines.

## Hardening

- Credential files written by swapdex are mode `0600`; the store dir is `0700`.
- Writes are atomic: the temp file is created `0600` in the destination's own
  directory and renamed into place; a cross-filesystem rename fails loudly
  rather than falling back to a non-atomic copy.
- Symlinked credential destinations are refused; running as root for a
  credential operation is refused.
- `use` backs up and verifies the current login before overwriting it.
- Credential bytes live only inside a `Secret` type that redacts on
  `Debug`/`Display` and zeroizes on drop. No command prints a token or a home
  path; the MCP surface is read-only with a field allowlist.

## Reporting a vulnerability

Please open a private security advisory on the GitHub repository
(Security -> Report a vulnerability) rather than a public issue. Include the
version, platform, and reproduction steps. We aim to acknowledge within a few
days.
