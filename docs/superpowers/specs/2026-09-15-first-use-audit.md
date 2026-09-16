# First-use audit

## Goal and authorization

Continue the user's authorized bug fixes, publication, installation and testing
from the perspective of someone installing Swapdex for the first time. Preserve
real credentials, account selections and native sessions while reproducing
onboarding problems in disposable homes.

## Observed problems

- A Codex-only machine cannot install its shim: `swapdex shim` exits 1 because
  Claude discovery runs first. Accepting the onboarding prompt fails the same
  way, and the prompt mentions Claude even when only Codex is installed.
- A fresh setup summary promises next-message switching without establishing
  that the native session uses a managed proxy. The README starts with two
  snapshots of the current login and mixes this legacy path with permanent
  slots, direct login launches, and serving-account changes.
- The shell installer states that it verifies release checksums but permits
  empty checksum responses or missing verification tools. Verify its actual
  failure behavior before changing it.

## Design

Install Claude and Codex shims independently using the existing native-tool
resolver. Report each installed tool and its actual PATH precedence. A missing
optional tool must not abort installation of the other; when neither exists,
give an actionable failure without writing misleading configuration. Onboard
only offers relevant missing shims. Codex-specific command hints retain their
tool selector.

Use existing account and launch commands; no new wizard or account model is
needed. Document a complete first-run path for each supported slot-capable tool:
native login into an account slot, shim activation, launch default selection,
plain native launch through the shim, then serving-account selection for the
next managed request. Distinguish direct named launches, saved snapshots and
managed sessions. State when a pre-existing direct session needs relaunching.

The installer must verify a valid checksum before replacing a working binary
and must not report a usable install when its candidate cannot execute. Keep
the existing package channels and Unix platform support.

## Verification

Reproduce each behavioral regression before repair. Cover empty, Claude-only,
Codex-only and combined installations; cancellation/non-interactive prompts;
existing logins; quoted paths; missing tools; checksum/download failures; and
the documented login-to-first-managed-request path with fake native tools and
loopback providers. Run the repository checks and installed routing/resume
verifiers before release. Record any unexercised real login or billing scope.
