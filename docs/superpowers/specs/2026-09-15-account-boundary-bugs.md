# Account boundaries during re-login and managed launch

## Problem and required behavior

The user expects a selected account to remain the account used by the next
request. Re-login must refresh that account's saved copy. Investigation of
0.162.0 found these boundaries to investigate and repair:

1. A Claude capture redirected to a slot reads the slot credential but the
   default home's `oauthAccount`. Its macOS credential reader can also fall
   back from an unavailable slot Keychain item to a different live login.
   Slot login inherits a secure-storage override that can redirect the child
   away from its own slot.
2. The Codex and Claude launchers continue with the native home's login when
   proxy startup fails, and accept any nonempty startup output as a port.
3. An existing full-suite run intermittently refused an immediate account
   selection as busy. Investigate whether a forked child retaining a duplicate
   file descriptor keeps the store lock alive after its Rust guard has ended.
   Require a deterministic regression before changing this boundary.

## Design

Keep Claude's explicit slot context in `Paths`, and use it for both identity
metadata and credential capture. Explicit slot captures use the existing
authoritative slot credential reader. A locked or missing macOS item must
return an error instead of reading another Keychain item or a leftover file.
Sandbox roots continue to use their local files. A managed Claude slot login
clears an inherited `CLAUDE_SECURESTORAGE_CONFIG_DIR`, so the child writes to
the same slot that capture and health inspect.
Rooted library callers must also skip the machine Keychain during ordinary
capture, apply and journal recovery; they need not set a process environment
variable to establish that boundary.

The identity path must also honor a nonempty `CLAUDE_CONFIG_DIR` passed when
resolving ordinary live paths: custom config uses `<config>/.claude.json`,
whereas an implicit default uses `<home>/.claude.json`. Record whether the
directory was explicit; comparing directory strings loses the distinction
when a user explicitly supplies `<home>/.claude`. Preserve a live caller's
intentional secure-storage override outside managed slot sign-in/capture.
Apply the managed child environment rule to `run_account` as well as picker
sign-in. A named `run` is an explicit launch in that slot and must resolve the
native executable, like picker login already does. Re-entering the installed
shim would block first login in an unsigned slot behind managed startup, or
route that explicit launch through another selected payer.
If no native executable can be resolved, fail with a clear diagnostic instead
of looking up the shim's bare name again. Existing active and serving pointers
must remain unchanged by a named launch.

For a managed Claude or Codex launch, require both a successful proxy startup
command and a single decimal port from 1 through 65535 before executing the
native client.
Otherwise return a nonzero status with a short diagnostic and a command for
inspecting the proxy error. Preserve existing explicit backend/profile/remote
choices and authentication/help bypasses. Preserve session repair's existing
warning behavior and stable `openai` provider routing. A tool with no managed
account, or an explicit passthrough choice, must remain usable. Distinguish
that known state from an unreadable/invalid selection; uncertainty must not
authorize fallback to another login. Keep this decision in the existing Rust
state resolver instead of parsing a human-readable payer label in the shim.
The Claude authentication bypass must identify an actual command, not a word
inside prompt text or an option value. Prompts such as `claude -p login` and
`claude -- login` must retain the managed route; real authentication and help
operations remain available without a proxy.

If reproduced, release store locks explicitly when their guard ends. A retained
descriptor in an unrelated child must not prolong a completed operation, and
a genuinely active guard must still exclude a concurrent writer. Do not add
retries that hide the ownership error.

Changing only the displayed profile label would leave mixed snapshots in the
store. Warning while using native authentication would still allow spending
on an unintended account. Correct the source and execution boundaries instead.

## Verification and limits

Use synthetic accounts and sandbox roots. First show failures for mixed slot
metadata, unavailable slot credentials and failed/invalid proxy startup.
Exercise the generated shell script, not only string matching. Cover valid
ports, explicit choices, slot metadata missing/corrupt, and unchanged default
home behavior. Verify the login child's effective secure-storage environment.

Run Rust tests, clippy, formatting, Python automation tests, npm tests,
dependency policy and audit checks. Platform CI must verify macOS; a Linux
test of source-selection policy does not prove real Keychain interaction.

The deliverable is a verified feature branch and reviewable pull request with
changelog and check results. Existing running services and saved credentials
are separate installation/repair targets; source verification alone must not
be reported as an installed fix.
