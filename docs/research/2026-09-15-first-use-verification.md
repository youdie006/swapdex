# First-use audit: 0.165.0

## Reproduced behavior

The installed 0.164.1 binary passed the prior routing verifier before this
audit. In a disposable home with only a fake Codex executable, however,
`swapdex shim` exited 1 because Claude was absent. Accepting onboarding's
Claude prompt failed the same way. The first-use integration regressions
recorded six expected failures before the repair.

The shell installer also accepted empty checksum responses and installations
without a checksum tool, and overwrote a working binary before discovering
that its replacement could not execute. A custom directory containing quotes
produced an unusable PATH hint. The installer fixtures reproduced the failures
without downloading a real release or replacing a real installation.

## Verification approach

`scripts/verify-first-use.py` follows the published quickstart using the actual
Swapdex executable, fake native sign-ins and a loopback provider. It exercises
Claude-only, Codex-only and combined installations, each from a fresh home and
an existing native login. Combined cases keep both native clients open and
check that a serving change for one tool leaves the other tool's payer alone.

The default mode verifies that a plain native command starts the proxy when
no proxy marker exists. It checks the first managed request, subsequent payer
changes, unchanged native process IDs and conversation homes, stable proxy
markers, preserved launch defaults and original native login bytes. Temporary
clients, detached proxies and provider threads are stopped after each case.
Linux adopts the detached fixture processes so they can be reaped explicitly.

Run it on a machine with unused default proxy ports:

```sh
python3 -B scripts/verify-first-use.py --swapdex /absolute/native/swapdex
```

On Linux, an isolated network namespace allows the same first-launch check
while normal Swapdex services continue running:

```sh
unshare --user --map-current-user --net --keep-caps /bin/sh -eu -c '
ip link set lo up
exec python3 -B scripts/verify-first-use.py --swapdex /absolute/native/swapdex
'
```

Installed-machine checks can instead pass `--proxy-mode foreground`. That mode
uses allocated loopback ports and checks the six account journeys, but prints
an explicit warning that proxy autostart is outside that run. Linux/macOS CI
runs the default autostart mode. Pass a native executable, not an npm wrapper.

## Local execution notes

- The repaired candidate passed all six foreground journeys and all six
  autostart journeys in an isolated Linux network namespace.
- The first namespace setup mapped the caller to UID 0 and hit Swapdex's
  intentional credential-operation refusal. Mapping the original user without
  retained namespace capabilities could not enable loopback. Keeping those
  namespace capabilities resolved the fixture setup; production guards were
  unchanged.
- One inspection command mistakenly placed backticks in a double-quoted shell
  search pattern and invoked a bare native Claude process through command
  substitution. It was interrupted with exit 130 before any output or prompt
  was observed. Subsequent process checks found no surviving command or child.
  No credential content was inspected or printed, and no evidence of user-state
  changes was observed. This was an audit-command error, not a product test.

These fake sign-ins verify slot isolation and invocation arguments; they do
not exercise a provider's browser/device login UI or independently inspect a
billing ledger. Final repository checks, publication channels, exact installed
versions and running-service verification are recorded in the version-specific
GitHub release and its PR after deployment.

## Candidate repository checks

- PASS `cargo test --all --locked --jobs 2`: 1,154 passed, zero failed; one
  existing large-streaming test remains ignored. The full suite was rerun
  after the final empty-account guidance change.
- PASS all-target locked Clippy with warnings denied, formatting and diff
  checks; 22 Python automation tests and seven npm launcher/publication tests.
- PASS dependency policy, bundled TLS-root checks and `cargo audit`.
- PASS first-use autostart: six journeys, with both native clients remaining
  open during the combined-tool checks.
- PASS installed-style routing: named runs, generated shims, persistent
  Claude/Codex A-B-A selection, accepted POST count one and safe GET recovery.
- PASS stock Codex provider repair, native listing/resume and HTTP fallback,
  with unchanged synthetic conversation bytes.
- PASS POSIX shell syntax under `sh`, `dash` and `bash --posix`; ShellCheck was
  unavailable on the local host. The installer integration suite passed 12 cases.
- Independent final code/spec review found no blocking regression.

## macOS CI path-alias correction

The first PR CI run passed on Linux but failed the new verifier on macOS:
https://github.com/youdie006/swapdex/actions/runs/34962007383

The verifier compared a native home path literally with a canonicalized slot
path. macOS can expose the same temporary directory through `/var` and
`/private/var`; this was a verifier assertion error, not lost slot isolation.
A Linux home symlink reproduced the exact failure before repair. Both native
home comparisons now resolve aliases before comparing. Every journey now uses
an aliased home, so Linux also exercises the condition. Native PID and home
stability within a conversation still use the original unmodified values.

After this correction, all six aliased-home autostart and foreground journeys,
Python lint and all six release-metadata checks passed locally. Fresh PR CI
is required before integration.
