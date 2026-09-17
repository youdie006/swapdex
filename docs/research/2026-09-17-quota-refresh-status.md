# Stale quota readings in an open dashboard

## Reproduction

On the affected WSL installation, the open picker and both managed proxies
matched the installed 0.165.6 native executable. The picker was not in terminal
scrollback mode. Four displayed accounts agreed with a separate current quota
read, and the cache continued to receive new observations.

One Claude account retained an eleven-hour-old reading. Its slot access was
expired, and the owning native session had not supplied usable access. The
background renewal log reported deferred renewal because a live native session
held the credential. This was not evidence of an OAuth-server rejection.

The dashboard copied the cached figures over the failed read and erased its
failure note. Its group summary also counted the expired account as ready and
included its cached headroom. Together these made a blocked read look like a
dashboard that had stopped refreshing.

## Resulting behavior

- Keep the failed-read explanation beside cached figures and their original
  observation age. An expired held slot explains that renewal is deferred to
  the native session. An ordinary expired slot gets slot-specific guidance.
- Exclude missing, expired, warned and paused accounts from ready count,
  headroom and reset forecast. Preserve spent and extra-usage handling for
  otherwise usable accounts.
- Keep quota inspection read-only. The explanation reuses the existing
  credential accessor and ownership guard, including Keychain support; it
  does not renew credentials or bypass native ownership.
- Accept a usable native login only through the existing same-account identity
  verification. The open picker can use it on the next background quota read.

## Verification record

The released 0.165.6 binary was reproduced in an isolated PTY using synthetic
accounts, a fake usage endpoint and a fake native credential holder. It showed
`3/3 ready · 51% left`, retained an eleven-hour-old reading, and omitted the
deferred-renewal reason. No OAuth requests occurred and slot credentials stayed
unchanged.

The first 0.165.7 candidate passed the same-process recovery check at 200
columns: it initially showed `2/3 ready · 74% left` with cause and age, then
read newly usable same-account native credentials after 45.88 seconds. The
picker PID stayed the same and no OAuth or real model requests occurred.

A subsequent 144-column check failed because the longer failure explanation
clipped the observation age. That result was rejected before publication;
overflowing notes now wrap below the gauges, and mouse selection uses actual
rendered item heights.

The final candidate passed the complete PTY sequence at both 144 and 80
columns. A VT screen emulator and inspected screen renders confirmed the
cause and age remained visible. After a valid synthetic native login became
available, the same picker process showed fresh figures after 45.67 and
45.65 seconds, respectively. Both runs kept slot credentials unchanged and
sent zero OAuth or real model requests. The 0.165.6 baseline reproduced the
original error with the same emulator. An earlier narrow-screen assertion
expected `% left`; it was corrected to accept the existing narrow `%` format.

Final source checks:

- `cargo test --all --locked`: 1,236 passed, 2 intentionally ignored, 0 failed.
- `cargo clippy --all-targets --locked -- -D warnings`: passed.
- `cargo fmt --all -- --check`, `cargo build --locked`, generated man-page
  parity, version synchronization and `git diff --check`: passed.
- `python3 -B -m unittest discover -s .github/scripts -p test_deps_automerge.py`:
  22 passed.
- `node --test 'npm/**/*.test.mjs'`: 7 passed.

All source execution used an isolated `SWAPDEX_ROOT`. Independent requirement
and code reviews accompany the PR. Publication, target installation and
running-service results belong in the version-specific release record.

## Operational limits

These changes do not make an expired credential usable. The owning native
session must refresh or obtain a valid login before that account's usage can
be read. No real credential renewal or native-session termination was used
in this investigation. Other machines were not assumed to share the same
credential condition.

An already-running picker retains the executable it loaded. Applying new
program code requires reopening that picker once; subsequent usage and login
changes use the background refresh. Software self-update is separate from
the stalled-read behavior reproduced here.

## Separate native-session authentication failure

After a fresh slot login, the user clarified that the picker was healthy but
existing Claude conversations still said `Login expired`. A fresh usage GET
with the slot credential succeeded. Eight existing native processes all used
the default Claude home, whose OAuth access and refresh fields were empty and
whose access deadline was zero. Native `claude auth status --json` reported
`loggedIn: false` there and `loggedIn: true` in the newly authenticated slot.
This was a separate native-login failure, not stale dashboard presentation.

The installed Claude 2.1.274 implementation contains a dead-refresh-token path
that writes that empty-field pattern after an invalid grant. This explains the
file shape; it does not establish which earlier exchange invalidated the token.
Its ordinary OAuth refresh check also probes credential-file modification time
and invalidates the cached login when the file changes.

At 2026-09-17 01:01:58 UTC, the empty default OAuth block was restored from the
new login only after matching both account and organization identity. The
operation retained unrelated MCP credentials, saved a mode-600 local backup,
and checked that neither the source slot nor account-selection pointers changed.
The repair helper was exercised against synthetic identities, mismatch cases,
an already-populated destination and an atomic file write before use. Native
authentication then reported logged in. All eight original native PIDs and
both existing proxy services remained alive; no model request, OAuth exchange
or native-session termination was used for the repair.

This was a local installation repair, not an automatic re-login feature in
0.165.7. Fresh-process authentication was verified; a successful response in
each already-running user conversation was not inferred from that check.
