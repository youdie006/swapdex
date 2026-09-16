# Post-release account and launcher audit

## Required behavior

Continue the user's authorized bug audit after installing 0.163.0. Preserve
the account selected for the next request, independent refresh ownership,
recoverable session discovery and launchers that work from any directory.
Use synthetic credentials and disposable roots for reproductions.

Confirmed failures:

- Relative PATH entries produce launchers that fail outside the install
  directory; a non-executable earlier entry can shadow a working native CLI.
- A rooted shim installation edits the ambient home's shell profile.
- A systemd executable path containing spaces is split, and launchd paths
  containing XML metacharacters are emitted without encoding.
- The generated `/swap NAME` instruction changes the launch-home pointer,
  while its interactive route correctly changes only the serving account.
- Distinct Codex subjects in one workspace share renewal claims and manual
  renewal deduplication.
- A quota cache entry containing only future reset times is discarded.
- An unreadable same-name Claude slot falls back to a previous profile's
  credential; selecting another slot can also hide the unrelated native row.
- The native resume menu limits files before account attribution, hiding older
  matching sessions. Directory aliases and cycles repeat the same transcripts.
- A timed-out session index command and its child remain running after return.
- Configured Codex failover can consult the Claude registry. A late failure
  from an old request can overwrite a newer explicit account selection.
- Both proxy retry layers replay an accepted POST when the upstream resets
  its connection before responding.

## Implementation boundaries

Anchor relative native CLI paths at discovery time, retaining symlink paths
so later native upgrades still take effect. Use the supplied `Paths` for
shell-profile and service lookup. Generate supervisor-specific path encoding
and teach service diagnostics to recover the actual executable. Route both
forms of the generated swap instruction through `serve`.

Reuse Codex's existing subject plus workspace identity for complete JWT
credentials. Preserve existing conservative fallback behavior when that
identity cannot be established. Copies of one rotating token must still share
exclusion when one copy has incomplete metadata; use a hashed token alias and
retain credential generations for retry decisions. Retain reset-only cache
entries until their windows expire. Keep slot credentials authoritative and
show unrelated native usage separately.

Coordination generations cover complete credential bytes. Sharing a successful
Codex renewal additionally requires the current file to match the leader's
persisted result generation. Replacing or restoring old bytes cannot inherit
that success. A recent success for the same rotating token conservatively
defers a changed blob; a different token remains independent.

Filter native sessions by account before bounding the result, visit each
physical directory once, and terminate timed-out index process groups. Tie
automatic routing writes to the observed selection generation, serialize those
writes, and bound reselection when the user changes or disables serving during
measurement. Select fallback accounts from the request's provider. Retry a
body-bearing transport failure only when it establishes that the request was
not sent; keep existing explicit authentication recovery behavior.

## Acceptance

Record regression failures before fixes, then run generated launchers and
service parsers against fake installations. Run repository Rust, lint, format,
Python, npm and dependency checks. Review and publish versioned changes;
installation and running-service verification remain separate release steps.
