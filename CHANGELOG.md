# Changelog

All notable changes to swapdex are documented here. This project follows
[Semantic Versioning](https://semver.org) and
[Keep a Changelog](https://keepachangelog.com).

## [0.27.0] - 2026-07-15

### Added
- **`swapdex sync-mcp`** shares your MCP servers across slots. `settings.json`
  and global `CLAUDE.md` are symlinked into each new slot automatically, but MCP
  config lives in the per-account `.claude.json` (mixed with the account
  identity), so it is shared with an explicit merge: the `mcpServers` block from
  `~/.claude.json` is copied into every slot's own `.claude.json`, preserving
  each slot's `oauthAccount`. Run it after logging into your slots.

### Fixed
- **The `claude` shim never bakes a self-reference.** Re-running `swapdex shim`
  with the shim dir already on `PATH` could pick the shim itself as the "real"
  claude and create an exec loop. It now skips any `claude` that carries the
  shim marker, robust against `~`/symlink/relative `PATH` spellings.

### Docs
- README and `docs/COMMANDS.md` now document the permanent-slot model end to end
  (`run`, `use` repoint + the `claude` shim, `onboard`, `adopt`, `migrate`,
  `sync-mcp`), with the classic snapshot commands kept as the coexisting path.

## [0.26.1] - 2026-07-14

### Fixed
- **Guided onboarding now runs automatically on first launch.** A bare `swapdex`
  on an interactive terminal offers the guided setup when there is something to
  set up (existing `~/.claude-*` dirs to register, or legacy profiles to
  migrate), instead of requiring you to already know the `swapdex onboard`
  command. It is shown once (a marker prevents re-nagging), then a bare `swapdex`
  drops into the normal picker. Non-interactive shells (pipes, scripts) are never
  hijacked - they still print the banner.

## [0.26.0] - 2026-07-14

The **permanent-slot account model**: each account lives in its own
`CLAUDE_CONFIG_DIR`, so switching never copies a token - and therefore can never
log an account out when a running session refreshes it. This is the complete fix
for the rotation-logout class that the 0.25.0 guard only *prevented*. swapdex
never writes a credential in this model; each account's own login creates and
refreshes its token in place.

### Added
- **`swapdex run <name>`** - launch Claude in an account's own permanent slot.
  Concurrent-safe: each terminal picks its own account, and they never collide.
- **`swapdex use <name>`** now repoints a lightweight default-account pointer for
  slot accounts (no credential copy). **`swapdex shim`** installs a `claude`
  shortcut that follows it, so a plain `claude` uses your default account.
- **`swapdex onboard`** - guided setup that registers existing `~/.claude-*`
  config dirs, migrates legacy profiles, and offers the shim, one prompt at a
  time. It never mentions "slots" - just gets you to a safe state.
- **`swapdex adopt <name> <dir>`** - register an existing `CLAUDE_CONFIG_DIR`
  directory as an account, in place (not moved).
- **`swapdex migrate`** - give each legacy copy-model Claude profile its own slot.
- **`swapdex slots`** lists the account slots; `doctor` now reports the slots, the
  default account, and whether the shim is installed.
- New slots inherit shared config (`settings.json`, global `CLAUDE.md`) from
  `~/.claude` via symlink, so switching accounts keeps your tooling. Tokens and
  history stay per-slot.

### Notes
- The legacy copy-switch (`use` on a profile that is not a slot) still works,
  guarded by the 0.25.0 running-session check, until you migrate.
- MCP server config lives in the per-account `.claude.json` and is not yet shared
  across slots.

## [0.25.0] - 2026-07-14

Prevents a real logout: switching a Claude account while a `claude` session is
still running on that same login slot. Confirmed on a multi-`CLAUDE_CONFIG_DIR`
macOS setup, cross-checked by a two-model design review.

### Fixed
- **`swapdex use` now refuses to switch Claude while a session is running on
  that login slot.** Claude's OAuth refresh tokens rotate (each refresh revokes
  the previous one). If a `claude` session keeps running after a switch, its
  next refresh rewrites the slot's token and revokes the snapshot swapdex just
  saved for the outgoing account - so switching back later logged that account
  out. swapdex now detects a running `claude` and the login slot it uses (its
  `CLAUDE_CONFIG_DIR`), and refuses the switch when a session is on the very
  slot being swapped. A session on a *different* `CLAUDE_CONFIG_DIR` profile is
  correctly ignored. If the running session's slot can't be read, it fails
  closed (refuses) rather than risk the logout.

### Added
- **`swapdex use --force`** to switch anyway when you know the running session
  is on a different account, or you have quit it. The refusal message names the
  flag and the risk.

## [0.24.6] - 2026-07-14

Onboarding and TUI robustness across less-common conditions (empty sessionwiki
index, a corrupt single-tool login, a concurrent `swapdex rm`), from a
cross-model review of the onboarding and menu paths.

### Fixed
- **An empty sessionwiki index no longer hides your real sessions.** When
  sessionwiki was installed but never `sync`ed (or its index was genuinely
  empty), `swapdex ui`'s "open a conversation" menu stopped at the empty
  sessionwiki result and showed a blank list. It now falls through to the
  native on-disk reader, so the sessions you can see on disk still appear.
- **The post-switch "open" prompt only offers tools the profile holds.** A
  Codex-only profile used to offer "c new claude"; pressing it opened your
  unrelated live Claude account in a new conversation. The plain menu now
  filters the new-conversation keys to the switched profile's own tools, the
  same rule the full TUI already applied.
- **`swapdex setup` no longer aborts when one tool's login is unreadable.** A
  corrupt or hand-edited credentials file for a single tool aborted the whole
  wizard before the other, valid tools were saved. It now warns for that tool
  and continues, just as it does for a tool you are simply not logged into.
- **The TUI no longer panics if a `swapdex rm` shrinks the list mid-session.**
  Switch, open, rename, and delete now use bounds-checked row access and clamp
  the selection after the row count changes, so a stale highlight can never
  index past the end.
- **Mouse clicks below the list no longer switch an account.** A click on the
  footer/help rows under the list box mapped to a hidden entry and could
  synthesize a switch; clicks are now confined to the list's inner area.
- **Mouse clicks on a scrolled session list open the row you clicked.** The
  click-to-row math ignored the list's scroll offset, so on a long, scrolled
  "open a conversation" menu a click opened an earlier, hidden session -
  possibly from another account. It is now offset-aware.
- **`o` (open a profile's sessions) switches to that profile first.** Opening a
  new conversation launches under whichever account is live, so pressing `o` on
  a profile you had not switched to opened your *currently live* account
  instead. `o` now switches first, exactly like Enter (it still differs by
  always showing the full menu rather than shortcutting a single-tool profile).
- **The welcome screen after deleting your last profile no longer claims you
  are logged out.** Deleting the final profile drops to the onboarding screen,
  which had stale "logged-in tools" state and hid the "save these" shortcut for
  a login the delete never touched; it is now recomputed after a delete.

## [0.24.5] - 2026-07-14

### Added
- **The native session menu now nudges you toward sessionwiki.** When
  sessionwiki is not installed, `swapdex ui`'s "open a conversation" menu shows
  a one-line footer: "install sessionwiki to search these, trace a file to its
  session, and group by account" - so you know the switching works today and
  what installing sessionwiki would add on top.

## [0.24.4] - 2026-07-14

### Fixed
- **Without sessionwiki, Codex sessions no longer show "(no prompt)".** The
  native session menu (`swapdex ui` when sessionwiki is not installed) read
  only Codex's old transcript shape; current rollouts carry the first prompt as
  `event_msg`/`user_message`, so every Codex session titled as "(no prompt)".
  It now reads all three shapes (event_msg, response_item, and the 2025-era
  bare message) and skips AGENTS.md / environment boilerplate - the same drift
  fixed in sessionwiki 0.19.3, now in swapdex's own native reader. Claude
  titles were unaffected.
- `swapdex sessions` without sessionwiki now points at `swapdex ui`, which
  lists recent sessions natively, instead of only saying "install sessionwiki".

## [0.24.3] - 2026-07-14

Hardening from a cross-model adversarial review (GPT-5.6 code pass + ChatGPT
Pro invariant pass). Both independently flagged the stale-file/Keychain issue
as the one a normal macOS user actually hits.

### Fixed
- **macOS: the authoritative Keychain is now read first.** swapdex leaves a
  `~/.claude/.credentials.json` behind on switch, but Claude refreshes its
  token in the *Keychain* (rotating the refresh token) without rewriting that
  file. Reading the file first handed back a stale, possibly-revoked token -
  and the switch-away writeback then persisted it into the profile, losing the
  live login. When the Keychain is in play it is now the source of truth; the
  file is only a fallback.
- **`swapdex quota` no longer leaks the account token to a curl trace.** curl
  reads `~/.curlrc` even with `--config -`; a user `curlrc` with `verbose` or
  `trace-ascii` could log the `Authorization: Bearer` header. curl is now run
  with `-q` first (disables curlrc), and the `SWAPDEX_CURL` test hook is
  honored only under `SWAPDEX_ROOT`, never in production.
- **macOS: switching/sign-out never touches another profile's Keychain item.**
  Keychain writes and deletes now target only the item this environment
  *derives* (never a discovered one), so a plain `swapdex` with a single
  aliased `CLAUDE_CONFIG_DIR` login can no longer overwrite or delete that
  alias profile.
- **A switch never overwrites a recoverable login it could not back up.** If
  the live login is valid (identity resolves) but a sibling file is corrupt
  (a hand-edited `~/.claude.json`, a broken Gemini `google_accounts.json`),
  `use`/`restore` now refuse for that tool and point at the repair instead of
  overwriting it with no backup. A genuinely broken login (unparseable) still
  gets replaced, as before.
- **`rm` and permission-tightening never follow a symlink out of the store.**
  A symlink planted inside the 0700 store could make secure-overwrite or chmod
  escape to an external file; the traversal now uses `lstat` and skips symlinks.
- **`swapdex quota` shows a sane reset countdown even if the endpoint returns
  milliseconds.** A 13-digit `resets_at` is normalized to seconds instead of
  rendering "resets in 21970092d".

## [0.24.2] - 2026-07-14

### Fixed
- **`swapdex setup` no longer lets you name a profile `-`.** The name `-` is
  reserved for `swapdex use -` (toggle to the previous profile), and `add` /
  `rename` already reject it - but setup's interactive save bypassed that
  guard, so answering `-` at the name prompt created a `-` profile that then
  broke `use -` ("can't tell which profile '-' means"). The shared name
  prompt (`ask_name`, used by setup, `login`, and interactive `add`) now
  rejects `-` and re-asks, matching the non-interactive commands.

## [0.24.1] - 2026-07-14

Two real-use bugs from an adversarial scenario sweep (12 sandboxed
workflows + live checks on a real multi-profile Mac).

### Fixed
- **`status` no longer cries "access token expired".** An OAuth access token
  lapses about hourly and the tool refreshes it silently; `status` still
  flagged every just-lapsed token with "access token expired, may re-prompt".
  This was the `status` twin of the 0.20.0 ls/marker fix - the same line, in a
  code path that fix missed. Now only a login older than 30 days (whose
  refresh token may actually be dead) gets a soft note. This is the daily
  false alarm behind "it keeps saying expired".
- **`add` on a corrupt `~/.claude.json` no longer claims you are "not logged
  in".** With a valid credential but a hand-edited config that has a JSON
  syntax error, `add` printed "not logged in to any selected tool" and exited
  3 - sending you to re-log-in when the real fix is to repair the file. It now
  exits 1 and points at the corrupt file (the detailed per-tool error was
  already correct; only the summary + exit code were wrong).

## [0.24.0] - 2026-07-13

### Changed
- **macOS Keychain resolution now mirrors Claude Code exactly - parallel
  CLAUDE_CONFIG_DIR profiles are finally safe.** Ground truth from a real
  multi-profile Mac (three live profiles: plain `claude` plus two
  `CLAUDE_CONFIG_DIR` aliases, three Keychain items) showed the root cause of
  "my switch did not stick" and "my other logins got wiped": the old
  suffix-preferring Keychain scan grabbed an ALIASED profile's item while the
  user's plain `claude` read the bare one - so switches wrote the wrong item,
  and add-account's cleanup deleted the wrong profile's login (plus the bare
  one). The new contract: **swapdex manages the profile of the environment it
  runs in**, derived the same deterministic way `claude` derives it (no env ->
  the bare/default item; CLAUDE_CONFIG_DIR set -> that profile's suffixed
  item). The scan remains only as a fallback when the derived item does not
  exist AND exactly one Claude login exists (alias-only setups); with several
  items it refuses to guess instead of corrupting another profile.
- Add-account's Keychain cleanup now deletes ONLY the resolved item - the old
  "also clear the bare name" extra could kill a live default profile.
- A fresh Keychain write (first switch on a Mac, or right after a sign-out)
  now creates the slot the environment derives, never a discovered one.
- `doctor` describes coexisting profiles truthfully: other Claude items are
  reported as "other CLAUDE_CONFIG_DIR profiles (or leftovers) - swapdex never
  touches them", not as stale strays to delete; a refused ambiguous resolution
  comes with the exact way out.

## [0.23.1] - 2026-07-12

Hardening release from an adversarial review of 0.21.0-0.23.0 (one reviewer
agent + a manual pass; 11 findings, the ones that matter fixed here).

### Fixed
- **macOS: SWAPDEX_ROOT now really isolates.** SWAPDEX_ROOT redirected every
  FILE path into a sandbox but Keychain writes still hit the machine-global
  login Keychain - a sandboxed test switch on a Mac could overwrite the REAL
  Claude token. All Keychain operations are now disabled under SWAPDEX_ROOT
  (file-only, like Linux).
- **Keychain resolution: exact match first.** When swapdex sees the same
  CLAUDE_CONFIG_DIR that `claude` launches with, the computed suffixed service
  name is used directly; the dump-keychain scan (which can pick a stale sibling
  when several suffixed items linger) is now only the fallback.
- **`doctor` keychain check has real teeth now.** 0.23.0's mismatch check could
  effectively never fire (the switch path resolves its target from the same
  scan). It now flags the two detectable causes: several suffixed items with no
  CLAUDE_CONFIG_DIR to break the tie (the scan can only guess), and the
  env-computed name disagreeing with the resolved target - each with the exact
  cleanup command.
- **Add-account sign-out verification is stricter.** It now also requires the
  credential to actually be GONE (not just the identity changed), so a residual
  second Keychain item can never lead to a profile pairing the OLD token with
  the NEW account's identity. Aborts and restores instead.
- **`quota`: a corrupt saved token no longer masquerades as "network down".**
  It is reported per-account ("saved token unusable") and the other accounts
  are still fetched; previously it could abort the whole run with a false
  "could not reach api.anthropic.com".
- **`quota --json` names are clean.** `name` no longer carries the " (active)"
  display suffix (the `active` field already says so) - safe to feed back into
  `swapdex use`.
- **curl is pinned** to /usr/bin/curl when present (PATH fallback otherwise) -
  the same PATH-shadowing discipline as /usr/bin/security - and a non-zero curl
  exit is now always a transport error (a partial body can not be parsed as a
  response).
- TUI: mouse wheel scrolls the doctor/usage/quota panels; `%` (quota) works
  before any profile is saved; a failed quota shows its error in the panel
  instead of rendering blank; a Down-key on an empty open-menu no longer
  underflows.
- README: the "macOS Claude is issue #1" install note was stale (Keychain
  switching shipped in 0.17-0.19); countdown format examples now match the
  actual output ("2h 14m").

### Added
- E2E tests for `quota` against a fake curl (SWAPDEX_CURL fixture hook, like
  SWAPDEX_SESSIONWIKI_JSON): clean JSON names, expired snapshots, and the
  no-false-offline behavior are now regression-locked. Unit tests for the
  countdown/bar renderers and the new keychain verdicts.

## [0.23.0] - 2026-07-10

### Added
- **`swapdex doctor` now diagnoses why a macOS switch "does not stick".** The
  #1 durable cause of "I switched but the old Claude account is still active" is
  a Keychain service-name mismatch: swapdex writes one item while Claude Code
  reads another (the suffixed `Claude Code-credentials-<hash>` that appears when
  CLAUDE_CONFIG_DIR is set). Doctor now shows Claude's real Keychain item(s),
  the one swapdex targets, and - on a mismatch - the exact fix (launch swapdex
  with the same CLAUDE_CONFIG_DIR you launch `claude` with). Read-only, macOS
  only; reuses the existing keychain discovery, so nothing new touches a
  credential. This turns a silent failure into a self-serve, actionable finding.

## [0.22.0] - 2026-07-10

### Fixed
- **CRITICAL: add-account no longer signs you out of your other accounts.**
  0.19.0 made the add-a-new-account flow run `claude auth logout` to clear the
  macOS Keychain. That command REVOKES the OAuth token server-side, which killed
  the snapshot swapdex had just saved for the current account - and, because the
  refresh token is shared, could invalidate every saved profile for that
  account. The result was "all my logged-in accounts got signed out". Sign-out
  is now LOCAL only (clear the Keychain item + credential file, exactly what
  claude-swap and Symbioose do) - it never revokes, so a saved login is always
  restorable. A regression test asserts swapdex never invokes `claude auth
  logout` and that the previously-saved profile's token survives an add-account.

  If accounts were already signed out: re-login each once (`claude`, then
  `/login`), then `swapdex add <name> --update` to re-save the fresh token.
  Normal `swapdex use` between saved accounts never had this problem.

## [0.21.0] - 2026-07-10

### Added
- **`swapdex quota` - remaining balance per Claude account.** The one opt-in
  network command: it reads each account's remaining 5h/7d quota (and per-model
  weekly windows) from Anthropic's official OAuth usage endpoint, using that
  account's *own* access token. Read-only, and it spends zero message quota. The
  active account is always live; a saved account whose token has expired reports
  so rather than showing a stale number - swapdex still never refreshes tokens,
  which is the line between a switcher and a rotator. Also in `swapdex ui` under
  the `%` key, and `swapdex quota --json` (which includes the raw response for
  any unexpected shape).

### Changed
- The "no network, ever" claim is now stated precisely: the switcher has no HTTP
  client in its dependency graph (still CI-asserted) and never touches the
  network; the new opt-in `quota` command shells out to `curl` to read your own
  balance and is the sole, hand-invoked exception. README and the network badge
  updated to say so honestly.

## [0.20.0] - 2026-07-10

### Fixed
- **No more constant "expired".** Claude access tokens live ~1h and Claude
  Code refreshes them silently, but swapdex flagged every saved Claude
  profile `(expired)` the moment the access token lapsed. The marker (and
  the switch-time warning) now fire only for a snapshot older than 30 days,
  whose refresh token may actually be dead - matching Codex/Gemini/Antigravity.
- **Opening a conversation offers only the tools the account has.** A
  Claude-only profile no longer shows Codex/Gemini/Antigravity; a single-tool
  switch goes straight to that tool's folder browser. The session list also
  falls back to any-account when none are attributed (so the menu isn't
  empty), and the sessionwiki lookup timeout is 2s -> 5s.

### Added
- **Usage in the UI** (press `u`): tokens used per account, read locally.
  Labelled honestly - swapdex is no-network, so this is tokens USED on this
  machine, not the vendor's remaining quota.

## [0.19.0] - 2026-07-08

### Fixed
- **Adding a new Claude account now works on macOS.** The flow tried to clear
  Claude's Keychain item with an external `security` call, which is not
  ACL-authorized to do so reliably - so Claude stayed signed in and dropped
  you back into the same session. swapdex now uses Claude Code's own
  non-interactive auth commands: `claude auth logout` to sign out (Claude
  holds the Keychain ACL, so it actually clears the token) and `claude auth
  login` to sign in (just the OAuth step, no workspace-trust detour). Direct
  file/Keychain cleanup stays as a fallback for older Claude builds. Same on
  Linux/WSL.

## [0.18.1] - 2026-07-08

### Fixed
- **macOS Claude add-account: target the real Keychain item, and verify the
  sign-out.** swapdex now discovers Claude's Keychain item first (preferring
  the hash-suffixed entry - the real credential - over a bare stray) rather
  than trusting a computed name, since swapdex may not see the same
  `CLAUDE_CONFIG_DIR` the user launches `claude` with. And after the local
  sign-out the add-account flow verifies the account is actually cleared; if
  swapdex couldn't clear the Keychain it aborts with guidance and restores,
  instead of opening Claude straight back into the same session.

## [0.18.0] - 2026-07-08

### Changed
- **macOS Claude Keychain, done right** (from decompiling Claude Code's own
  bundle and reading the mature switchers). The Keychain service name is now
  COMPUTED exactly as Claude Code computes it - `Claude Code-credentials`
  plus a `-sha256(CLAUDE_CONFIG_DIR)[..8]` suffix when that env var is set -
  so swapdex targets the right item even when `CLAUDE_CONFIG_DIR` is set (the
  case that hardcoding tools get wrong), with runtime discovery as a
  fallback. All Keychain calls go through `/usr/bin/security` (the same
  binary Claude used to create the item, so its ACL already trusts it - no
  "Always Allow" prompt), target the item by account (`$USER`), and pass the
  token as hex over stdin so it never appears in `ps`. Linux/WSL unchanged.

## [0.17.2] - 2026-07-08

### Fixed
- **macOS Claude Keychain: target the item by account, not service alone.**
  Reading/deleting Claude's Keychain credential matched by service name
  only, so a stray bare `Claude Code-credentials` item (an older swapdex may
  have written one) could be hit instead of Claude's real item, leaving
  Claude logged in. Read and delete now pass `-a <account>` (the item's own
  account, else `$USER`) to target exactly Claude's credential, and delete
  also clears a distinct stray. Confirmed against Anthropic's auth docs and
  the community switchers: the macOS credential is the Keychain item plus
  the `oauthAccount` block in `~/.claude.json`, and `CLAUDE_CONFIG_DIR` does
  not isolate it on macOS - a Keychain swap (what swapdex does) is correct.

## [0.17.1] - 2026-07-08

### Fixed
- **macOS Claude Keychain: use the REAL service name.** 0.17.0 assumed the
  Keychain service was exactly `Claude Code-credentials`, but Claude's item
  has a per-install hash suffix (e.g. `Claude Code-credentials-5953ba74`), so
  swapdex operated on the wrong item and Claude stayed signed in. The service
  name is now discovered at runtime from the login keychain's attributes
  (no password prompt) and read/write/delete target it. On first access
  macOS will ask to allow swapdex to read the item - choose "Always Allow".

## [0.17.0] - 2026-07-08

### Added
- **Claude Code account switching on macOS** (issue #1). Claude on macOS keeps
  its login in the login Keychain rather than a file, so swapdex previously
  refused to switch it there. The Claude adapter now reads and writes the
  Keychain (via `security`): `capture` reads the token from the file or the
  Keychain, `apply` writes it to both plus the `.claude.json` identity with
  all-or-nothing rollback, and the add-a-new-account flow deletes the Keychain
  item so Claude prompts a fresh sign-in. Linux and WSL are unchanged
  (file-based); the Keychain code is a no-op off macOS.

## [0.16.3] - 2026-07-08

### Fixed
- **A left-open `swapdex login` no longer locks the whole store.** The
  add-a-new-account flow held the store lock across the interactive tool
  sign-in (which can take minutes or be left open), so while it was open
  every other operation - rename, use, restore - failed with "another
  swapdex is mid-switch". This was the macOS "rename doesn't work" report:
  a half-finished login had permanently locked the store. The lock now
  covers only the store writes and is released during the sign-in. The busy
  message also names the likely cause.

## [0.16.2] - 2026-07-08

### Fixed
- **Renaming in the UI now mutates the store directly** instead of shelling
  out to a `swapdex rename` subprocess. The subprocess resolved the binary
  via `current_exe()`, which can misbehave under some installs/wrappers and
  make the rename a silent no-op while the UI still refreshed and looked
  fine. It now renames in-process with the same validation, lock, and
  collision check as the CLI.

## [0.16.1] - 2026-07-08

### Fixed
- **Adding a new account that signs you back into the SAME one.** swapdex
  removes the local login and opens the tool, but it cannot make the tool's
  OAuth show an account picker - with a live browser session, the tool signs
  you straight back into the same account. The old flow printed a note but
  still saved that account under the new name, leaving a duplicate profile
  and no actual new account. Now it saves nothing under the new name,
  restores the login as it was, and explains per-tool how to reach the other
  account (sign out at claude.ai / chatgpt.com, or pick the other Google
  account) - printed both up front and if it happens.

## [0.16.0] - 2026-07-08

### Changed
- **Opening a new conversation is now a folder BROWSER, not a text field.**
  You no longer type or memorize a path: each level lists its
  subdirectories, Enter/Right descends, Left/Backspace (or the `..` row)
  goes up, a `~ (home)` row jumps home, and `> open here` launches the
  conversation in the current directory. Fully mouse-driven too - scroll,
  click a folder to enter it, click "open here" to launch. Dotfiles are
  hidden and the current path is shown in the title.

## [0.15.0] - 2026-07-08

A full UI overhaul, by user request: the picker is now a designed interface,
not a plain list.

### Added
- **A logo header.** The two-tone `swapdex` wordmark (violet SWAP + dimmed
  dex - the same mark the CLI prints) crowns a rounded, violet-titled panel.
  The active profile shows a filled dot, plan tier and warnings are
  colour-coded, and the key hints render the keys in violet. The logo drops
  automatically on short terminals so the list keeps its room.
- **Every feature is reachable in the UI now**: `n` renames the selected
  profile, `?` opens a read-only `doctor` health panel (with a "checking..."
  frame so it never looks frozen), alongside the existing switch / open /
  add / restore / delete.
- **Onboarding.** An empty store opens a welcome screen that detects the
  tools you're already logged into and offers to save them as your first
  profile with one key (`s`). A bare `swapdex` opens this for a
  fresh-but-logged-in user too.
- **Mouse.** Scroll to move the selection, click a menu item to choose it,
  click a profile row to select it (Enter still performs the switch, so a
  stray click never switches by surprise).

Every UI action runs the same subprocess command path as the CLI, so there
is still exactly one implementation of each.

## [0.14.0] - 2026-07-08

Three more lenses (a threat-model security audit, a model-based random-walk
soak, and a distribution-surface pass) plus a direct user report.

### Changed
- **A bare `swapdex` on an interactive terminal now opens the picker** when
  you have saved accounts, instead of printing a banner that flashes and
  returns (which read as "it opened and closed"). Pipes, dumb terminals,
  and fresh machines still get the banner + hints, and a bare run never
  creates the store.

### Fixed
- **Security (symlink escape):** a symlinked `accounts/<name>` or store
  directory could redirect a credential write OUTSIDE the 0700 store - the
  symlink refusal only checked the final path component. Every store
  read/write now verifies each component under the store root.
- **Security (MCP):** the read-only MCP server no longer reflects an
  attacker-controlled tool/method name back into its JSON-RPC error text.
- Declining the `add --update` repoint prompt printed "not logged in to any
  selected tool" and exited 3 - a lie; it now says nothing was saved
  because you declined, and exits 0. (Found by the soak.)
- The CI "no network" guard is broadened from 5 HTTP-client names to also
  fail on tokio/rustls/native-tls/openssl/socket2/hickory/quinn/h2, so a
  future socket-capable dependency can't slip the "100% local" promise.

### Verified by the security audit (no changes needed)
- The usage cache holds no token text (only ids/timestamps/counts); error
  messages are secret-free even when the token itself is malformed; the MCP
  server is strictly read-only and exposes no token, uuid, or path; the
  atomic temp file is created 0600 with no widening window; `ensure_not_root`
  guards every credential-mutating entry point.

## [0.13.0] - 2026-07-08

Four new audit lenses (upgrade compatibility, environment torture, parser
fuzzing, docs-vs-behavior contracts) plus real-machine profiling.

### Performance
- **`usage` on a heavy machine: ~20s -> ~0.5s.** A heavy week holds ~1GB of
  transcripts inside the 7-day window; usage reparsed all of it every run.
  Files are now parsed once into a per-file events cache (keyed by
  mtime+size, pruned to the window, atomic 0600) and cache misses parse
  across up to 8 threads. Cached and uncached outputs are byte-identical.

### Fixed
- **A future-stamped backup no longer hijacks `restore`.** One switch under
  clock skew (NTP jump, VM resume) wrote a backup stamp that shadowed every
  real backup forever - restore could silently no-op or restore a stale
  THIRD account, and the ghost survived pruning. Stamps more than an hour
  in the future now sort as the oldest everywhere.
- An unwritable store says so ("store is not writable: ...") instead of the
  unwinnable "another swapdex is mid-switch; try again"; doctor-adjacent
  lock errors are distinguished from real contention.
- A legacy all-whitespace profile (0.2.x allowed creating them) is
  manageable again - the whitespace rule moved to creation time, like the
  `-` reservation, so `rm`/`rename`/`use` still work on it after upgrade.
- Two separate invocations inside one wall-clock second no longer collide
  in `restore`'s last-switch scoping: timeline events carry a
  per-invocation discriminator (legacy events fall back to ts grouping).
- `TERM=dumb` (or empty) on a real terminal gets the plain numbered prompt
  instead of raw ANSI escapes.
- The MCP server's oversized-line resync is constant-memory - a 200MB
  no-newline request used to allocate 200MB just to skip it.
- Seven doc/string drifts from the contract audit (76 contracts verified
  OK): the ui pipe-fallback claim, exit-code rows 2 and 3, the backup
  guarantee's unreadable-live exception, the ui --help text, the two-tool
  top help/banner, and the status sample's missing tier.

### Verified (no changes needed)
- Upgrade compatibility is fully clean: stores created by 0.2.1 / 0.5.0 /
  0.9.2 read perfectly (and 0.12-created stores read back on old binaries);
  timeline compaction stays bounded through 2,200 events; backups stay at
  2 per tool.
- Fuzzing: 890 mutants / ~3,000 invocations across all four credential
  parsers, store snapshots, timeline, native session files, MCP JSON-RPC,
  and every --json output - zero panics, zero hangs, zero secret leaks,
  zero wrong-account results.

## [0.12.1] - 2026-07-08

A delta audit on the bug-sweep itself (fixes breed bugs) plus the last
"observation" items.

### Fixed
- **The login repoint guard could be bypassed** when the target profile's
  saved snapshot was unreadable - corrupt and absent were conflated, so a
  corrupt snapshot let the new sign-in silently overwrite the profile. An
  unreadable snapshot now counts as "different" and asks.
- **Refusing a repoint no longer discards your completed sign-in.** You get
  to save the NEW account under a different name; only skipping that
  explicitly discards it, and the message now says so honestly (the old one
  claimed "keep both accounts" while destroying one).
- The interactive sign-in also rides out **SIGQUIT** (Ctrl+backslash), not
  just Ctrl+C.
- Ghost profile dirs (no known tools; hidden from `ls`) are treated
  consistently by `rename`: not a valid source (exit 5), and colliding with
  one as target is a clean "already exists" (exit 6, was a hard error).
- `usage` prints an honest note when gemini/antigravity are logged in -
  those CLIs keep no local token transcripts, and silence must not read as
  zero usage.
- setup skips a tool whose login cannot be read instead of aborting the
  whole wizard; the login flow's keep-name suggestion falls back to `main`
  when no email exists on disk; the ui shows what to do after the last
  profile is deleted instead of an empty box.

## [0.12.0] - 2026-07-07

The bug-sweep release: three independent adversarial audits (a fresh-user
walkthrough of every command, a logic review of the newest code, and the
add-a-second-account journey run for each tool) plus the unified login flow.
24 defects fixed, each with a regression test.

### The big ones
- **Adding a second account now truly works for ALL four tools.** The
  save-current / sign-out / fresh-sign-in / capture flow existed only for
  Claude; gemini and antigravity dead-ended in guidance whose instruction
  saved the WRONG account under the new name, and codex's "already logged
  in" no-op did the same silently. One tool-generic flow now, with
  automatic restore on any failure - including a shell Ctrl+C mid-sign-in,
  which used to leave you signed out of everything.
- **A corrupt live ~/.claude.json is diagnosed as such** - previously every
  switch blamed the profile snapshot, both suggested remedies failed, and
  doctor said everything was ok.
- **Multi-tool switches no longer abort on the first failing tool** - the
  others proceed and a summary names what failed (exit 1).
- **Enter-through setup saves all four tools** - the "replace it?" prompt
  silently skipped every tool after the first.
- **The ui no longer panics after deleting the last profile.**

### Also fixed
- login guards repointing an existing profile to a different account, and
  rejects the reserved name `-`; non-TTY login-while-logged-in exits 3.
- rename rewrites timeline attribution (usage/sessions no longer report a
  dead profile name forever).
- Multi-tool ls/ui prefer Claude's real plan tier over antigravity's
  auth_method; Antigravity saves print an honest "cannot confirm WHICH
  Google account" note (no identity exists on disk).
- doctor checks live credential file permissions for all four tools, its
  backups/tools lines cover all four, and it diagnoses corrupt
  .claude.json by name.
- A `use` typo prints one line; ls hides crash-debris dirs and unknown
  tool subdirs; whitespace-only names are rejected; the invalid-name
  message states the real rules; fresh-install apply failures clean up
  the half-written file; bare `~` expands in folder prompts; native
  session titles no longer drop real prompts starting with `<`.

## [0.11.0] - 2026-07-07

Deep account dig, round 2: the rotation invariant ("a profile always holds
this account's newest known login") now holds on EVERY path that touches the
live login, and a profile's identity can no longer change silently.

### Fixed
- **`restore` refreshes the outgoing account's profile** with its latest
  (possibly rotated) tokens before undoing a switch - the same stale-token
  fix 0.10.0 gave `use`.
- **A no-op `use` is now a sync point**: switching to the already-active
  profile refreshes its snapshot from the live login (tokens rotate while
  you work). No backup and no timeline event - nothing is switching.
- **`add --update` no longer silently repoints a profile** to a different
  account. Logged into B while updating a profile that holds A: on a
  terminal it asks; non-interactively it refuses with exit 7 and shows both
  the keep-both and the explicit-repoint commands. Same-account updates
  (the documented stale-token refresh) pass through unchanged.

## [0.10.0] - 2026-07-07

A deep dig into account handling itself.

### Fixed
- **Stale-profile token rotation** - the deepest account bug a switcher can
  have. Providers ROTATE refresh tokens while an account is in use, so a
  profile snapshot goes stale the moment you work on that account; switching
  away and back later could restore a refresh token the provider had already
  revoked, forcing a re-login and making the switch look broken. Now `use`
  (and the `login` flow's stash) write the outgoing live capture - the
  freshest known tokens - back into EVERY profile holding that account
  before switching. A profile now always means "this account's newest known
  login", not "the login as of the day you saved it".
- **Store permissions self-tighten.** Snapshots are tokens, and doctor's
  store check only looked at the top-level directory - `cp -r`, backup
  tools, or a loose umask could leave a world-readable token file inside
  unnoticed. Opening the store now walks it and re-tightens every dir to
  0700 and every file to 0600, best-effort, on every command.

### Verified in the same dig (no changes needed)
- Symlinked credential files are refused with a non-zero exit.
- Two profiles holding the same account both stay fresh under the new
  rotation rule; the active marker points at the first match.

## [0.9.2] - 2026-07-07

Another angle-testing round as a user (tiny terminals, Unicode names, wrong
keys, error paths, full journeys through a pty). Four fixes.

### Fixed
- **Ctrl+C now quits the ui** from any screen. Raw mode swallows the signal,
  so the key was silently ignored - and it is the first key a user in
  trouble reaches for.
- **setup's "add another account" step asks WHICH tool** (all four) and runs
  the same one-flow login. The old block was Codex-only - the root of "it
  keeps asking about Codex accounts" in real use.
- setup's intro line names all four tools, not "Claude Code / Codex".
- `login` without `--tool`: a wrong number at the tool question re-prompts
  instead of silently cancelling.

### Verified in the same round (no changes needed)
- 4-line terminals render without panicking; Unicode/CJK profile names align;
  `--open`/`--dir` error paths exit non-zero with clear messages; the full
  ui add-account journey returns to the picker with the new profile active.

## [0.9.1] - 2026-07-07

### Fixed
- Esc in the folder prompt goes back ONE step (to the conversation menu),
  not two - a double-tapped Esc could accidentally quit the whole ui.
  Found by driving the ui end-to-end as a user through a pty.

## [0.9.0] - 2026-07-07

Two more real-use asks, same day.

### Added
- **Sessions without sessionwiki.** The post-switch menu now reads recent
  sessions STRAIGHT from each tool's own store (`~/.claude/projects`,
  `~/.codex/sessions`) when sessionwiki is absent - titles from the first
  user message, resume via the tool's native mechanism (`claude --resume
  <id>` in the session's own folder, `codex resume <id>`). A session's
  recorded cwd is only trusted when it exists as a real local directory.
  sessionwiki, when installed, still provides the richer cross-tool view.
- **The ui stays up** (ccusage-style): one persistent full-screen session.
  Switching shows its condensed result in the status line and refreshes the
  list in place; `o` opens the conversation menu for the selected profile
  (recent sessions + new-conversation entries with an in-UI folder prompt);
  Esc returns to the list. Opening a conversation is the one action that
  leaves - that is the point of a switch. Internally a switch runs this same
  binary as a subprocess, so there is still exactly one switching
  implementation.

## [0.8.0] - 2026-07-07

### Added
- **Switch, land in a conversation.** The post-switch menu now opens the tool
  itself: pick a recent session by number to resume it in its own folder
  (via sessionwiki), or `c`/`x`/`g`/`a` to open a NEW claude/codex/gemini/agy
  conversation - it asks which project folder (Enter keeps the current one,
  `~` expands). And `swapdex use <name> --tool claude --open [--dir <path>]`
  does switch-and-launch in one command. Real-use feedback: switching is not
  done until the conversation is open.

## [0.7.0] - 2026-07-07

Real-use feedback release: the three things that actually hurt.

### Added
- **Add a NEW account in one flow**: `swapdex login <name> --tool claude`
  while already logged in now does the whole thing - saves your current
  login (profile + store backup), signs you out locally, opens Claude Code
  for the fresh sign-in, and captures the new account. If the sign-in does
  not complete, your previous login is restored automatically; it can never
  be lost. (Previously this case printed instructions and stopped - the
  single most-hit wall in real use.)
- **Full-screen `ui`** on a real terminal: arrow keys, Enter to switch, `a`
  add a new account, `r` restore, `d` delete (with confirm), `q` quit -
  the llmux-style experience, by direct request. Every action runs the
  exact same command path as the CLI; piped stdin falls back to the plain
  numbered prompt. (ratatui with the crossterm backend only; the "no HTTP
  client in the dependency graph" guarantee is unchanged.)

### Changed
- `login` without `--tool` ASKS which tool instead of silently preferring
  Codex when it is installed - the old guess kept steering Claude users to
  the wrong tool.
- Tool ordering everywhere (setup, ls, status, doctor) leads with Claude
  Code, then Codex, Gemini, Antigravity.

## [0.6.0] - 2026-07-07

### Added
- **Antigravity support** (Google's agentic CLI, binary `agy`): its token at
  `~/.gemini/antigravity-cli/antigravity-oauth-token` is a fourth switchable
  tool - one profile can hold Claude Code + Codex + Gemini + Antigravity and
  a single `use` switches all four. No email or account id is stored on disk,
  so the profile match uses a one-way fingerprint of the refresh token (a
  fresh re-login honestly degrades to "not saved" until you re-add).

### Changed
- Gemini's `ls` marker is `stale` (snapshot refreshed >30 days ago, like
  Codex) instead of `expired`: Gemini access tokens live about an hour and
  the CLI refreshes them silently, so "expired right now" was pure noise.

## [0.5.0] - 2026-07-07

Two headline features: a third tool, and per-account usage.

### Added
- **Gemini CLI support**: `~/.gemini/oauth_creds.json` +
  `~/.gemini/google_accounts.json` are switched together with the same
  both-or-neither rollback the Claude adapter uses. One profile can now hold
  Claude Code + Codex + Gemini and a single `use` switches all three;
  `--tool gemini` scopes any command; `ls`/`status`/`ui`/`doctor`/`restore`
  cover it like the others; sessionwiki's account badges pick Gemini sessions
  up automatically (the timeline join is tool-generic). `--tool all` is the
  explicit everything-selector (alias `both` kept for scripts).
- **`usage` is per-account once a switch history exists**: every token event
  is attributed to the profile active at its timestamp - the same honest join
  `sessions` uses - so "how much have I used on EACH account" finally has an
  answer. What predates your first switch shows as untagged; no history, no
  guessing. JSON grows an `accounts` object per tool.

## [0.4.2] - 2026-07-07

Ecosystem-walkthrough fixes: the integrated flows, from a fresh user's chair.

### Fixed
- The `ui` resume handoff passes `--no-sync`: on a large store the exec used
  to kick off a full index sync - minutes of progress spam that looked like a
  hang in the flagship flow. sessionwiki still self-syncs when the id is not
  yet indexed.
- A present-but-never-synced sessionwiki no longer reads as "0 sessions":
  `sessions` and `status` say "index empty - run `sessionwiki sync` once".
- The sessionwiki read cap rose 1000 -> 50000, so the status summary cannot
  silently understate a large store.

### Added
- `sessions --json`: {"available", "accounts", "total"} for scripting
  (available=false distinguishes "no sessionwiki" from "zero sessions").

## [0.4.1] - 2026-07-07

Fixes from an adversarial audit of the 0.4.0 delta.

### Fixed
- `ui` no longer panics on a session id with multibyte characters (the id
  prefix was a byte slice; now char-based).
- The "any account" continuity fallback now fires on the FIRST real switch -
  the very case it was written for. (The empty-timeline check ran after the
  switch had already written its own event, so it only ever fired on a no-op
  pick.)
- `exec` handoff passes the session id after a `--` separator, so an id that
  begins with `-` can never be parsed as a flag.
- The `SWAPDEX_SESSIONWIKI_JSON` test fixture hook is only honored together
  with `SWAPDEX_ROOT` - a stray env var can no longer redirect a production
  run.

### Docs
- The README demo now shows the full integrated loop: `ui` -> switch ->
  recent sessions -> resume handoff -> `status --short`.

## [0.4.0] - 2026-07-07

### Added
- `ui` completes the loop: pick a recent session by number after the switch
  and swapdex hands off to `sessionwiki resume <id>` directly (a one-shot
  `exec` of the official reopen flow - the session's own tool takes over the
  terminal). Enter skips; nothing ever launches unasked. This is the same
  precedent as `login` driving the official sign-in: an explicit hand-off is
  not a wrapper - swapdex `exec`s and is gone.

## [0.3.1] - 2026-07-07

### Added
- `ui` shows a continuity hint after the switch: the picked account's recent
  sessions (id, relative age, tool, title) with the one command to reopen one
  (`sessionwiki resume <id>`) - switch, land back in the work you switched
  for. Before the first recorded switch, when nothing can be attributed yet,
  it honestly falls back to the most recent sessions of any account and says
  so. Requires sessionwiki; silently absent otherwise.

## [0.3.0] - 2026-07-07

### Added
- `swapdex ui`: an interactive picker - every profile with its account,
  active marker, and the session summary; type a number to switch, Enter or
  `q` cancels. The selection runs the exact same safe `use` path (backup,
  validate, atomic apply), so a human picking a number IS the explicit
  switch - the no-auto-rotation bright line is untouched. Deliberately
  stdin-only: no raw-mode TUI crate, nothing socket-shaped enters the
  dependency graph.

## [0.2.2] - 2026-07-07

### Fixed
- `ls` aligns by display width, so a CJK profile name (two columns per
  character) no longer shears the table.

### Docs
- The `status --short` line drops straight into Claude Code's own status line
  (`statusLine` snippet in the README) - the active account stays visible
  inside the tool you are switching.
- An honest Alternatives section (claude-swap, aisw, caam) with each
  project's trade-offs and when to pick them over swapdex.

## [0.2.1] - 2026-07-06

Fixes from an adversarial audit of the 0.2.0 delta, plus scripting/completion
polish.

### Fixed
- `use ""` (an unset shell variable) matched a single profile as a "unique
  prefix" and performed a real switch; an empty name is now rejected (exit 2)
  with the live login untouched.
- `use -` can no longer re-pick the profile you are already on when the live
  identity is unreadable (the newest switch's destination is excluded); the
  refusal message says the real reason when both profiles are active; and
  `--tool` now scopes the `-` resolution.
- macOS Keychain-mode installs: a bare `use`/`restore` skips claude-code with
  a note and keeps switching Codex, instead of aborting the whole command.
- `doctor`: the store-permission check could never fire (the store self-heals
  its mode on open) - it now reports what it found; the expired/stale remedy
  says "log in to that account" first, so following it verbatim can no longer
  overwrite the profile with whatever account happened to be live.
- `rm` checks the profile exists before asking y/N.
- `manpage` failures exit 1 instead of printing nothing successfully.
- A legacy profile literally named `-` stays manageable after the upgrade
  (`-` is rejected only when creating/renaming).
- A bare `swapdex` no longer creates the store directory as a side effect.

### Added
- `ls --names`: bare profile names one per line; the docs gain a verified
  bash/zsh snippet that tab-completes profile names for `use`/`rm`/`rename`.
- `add` with no name asks on a terminal (name suggested from the live
  account); non-interactively it errors with the fix.
- `doctor` verdicts are colored on a TTY; NO_COLOR is respected everywhere.
- The demo GIF shows `use -`, `status --short`, and the colored doctor.

## [0.2.0] - 2026-07-06

Daily-driver ergonomics: the goal is a switch in two keystrokes and zero
guessing about where you stand.

### Added
- `swapdex use -`: toggle to the previous/other profile, like `cd -` /
  `git switch -`. With two profiles it is simply the other one; with more it
  is the profile you were on before (from the switch timeline). `-` is now a
  reserved name.
- Unique-prefix matching on `use`: `swapdex use w` resolves to `work` and says
  so; an ambiguous prefix refuses and lists the candidates (switching is a
  write - it never guesses).
- `swapdex status --short`: one compact `claude:work codex:personal` line for
  shell prompts and statuslines (starship/PS1 snippet in the README).
- A bare `swapdex` now shows the active accounts under the banner, so the
  naked command answers "where am I?".

### Changed
- `rm` asks y/N on a terminal instead of demanding `--yes`; scripts keep the
  explicit `--yes` requirement (exit 7 when stdin is not a tty).

## [0.1.9] - 2026-07-06

### Added
- `swapdex manpage`: prints the man page (roff) to stdout. Homebrew installs
  it - and shell completions - automatically.
- A demo GIF of the core loop (ls -> use -> status -> restore -> doctor) at
  the top of the README.

### Fixed
- `use`/`restore` no longer print the running-session warning under
  `SWAPDEX_ROOT`: an isolated root's credentials are not the ones a live
  session uses, so the warning was a false positive there.

## [0.1.8] - 2026-07-06

### Added
- `swapdex doctor`: local health check - store permissions, every saved
  snapshot (unreadable/expired/stale), both live logins (including the
  corrupt-file case), backups, `.claude.json` permissions, and the CLIs on
  PATH. Each finding ends with its remedy. Exit 0 healthy, 9 when problems
  were found. Local only, never the network.

### Changed
- The switch timeline file is bounded (compacts to the newest 1000 events)
  instead of growing forever.
- `add` hints about quoting when a profile name contains spaces.

## [0.1.7] - 2026-07-06

Findings from a two-track review (adversarial code audit + a new-user
walkthrough), all fixed and regression-tested.

### Added
- `swapdex restore [--tool ...] [--dry-run]`: put back the login that was live
  before the last switch. `use` has always backed up the outgoing login (even
  one never saved as a profile), but there was no command to bring it back - a
  bad switch meant hand-copying files. `restore` backs up the current login
  first, so running it again toggles between the two. A bare `restore` scopes
  itself to the tool(s) the last switch touched, and it skips a torn backup in
  favor of an older intact one.
- `use` warns when the OUTGOING login is not saved as any profile (only the
  last 2 backups remember it), and when a live session of the switched tool is
  running.

### Fixed
- `usage` was wrong in both directions: Codex was undercounted 10-100x (it
  read the per-request `last_token_usage` as if it were cumulative; now it
  windows the deltas of the monotonic `total_token_usage` by event time) and
  Claude was overcounted ~2.5x (one line per content block repeats the same
  `message.id`, and resumed sessions copy messages into new files; now deduped
  by message id). Also ~9x faster (streaming + pre-filter instead of
  whole-file reads; 12.2s -> 1.3s on a 927MB transcript set).
- A corrupt live credential file no longer blocks recovery: `use <profile>`
  warns and replaces it (previously it aborted - the one command that could
  fix the file refused to run), `status` reports "login file unreadable" per
  tool instead of dying mid-output, and `restore` tolerates it too.
- macOS: `use`/`add`/`restore` on a Keychain-mode Claude Code install now
  refuse with an explanation instead of half-switching (writing a credentials
  file the CLI ignores while flipping the reported identity). `status` and
  `add` explain the Keychain situation. Codex switching works on macOS.
- `rename` to an existing name exits 6 ("already exists", like `add`) instead
  of a generic hard error, and takes the store lock like every other mutation.
- `login <name> --tool claude` with no CLI on PATH exits 3 on stderr (was:
  exit 0 on stdout - scripts saw success where nothing was saved).
- A corrupt saved snapshot is now visible as `(unreadable)` in `ls` with a
  remedy footer, and a failing `use` names the profile and the fix.
- Claude apply: if the rollback after a failed config write ALSO fails (e.g.
  disk full), the error now says so instead of claiming a clean rollback.
- `restore` attributes its timeline event to the restored profile's name, so
  `sessions` no longer blames an account literally named "(backup)".
- setup: Ctrl-D (EOF) at a prompt exits cleanly instead of spinning forever.
- Timestamps with fractional seconds AND a numeric timezone offset
  ("...00.123+09:00") now parse the offset instead of ignoring it.

### Changed
- `use` prints "{tool}: profile 'x' has no {tool} login - left unchanged" when
  a logged-in tool is skipped, instead of silently half-switching; `--dry-run`
  shows the target account's email.
- `ls` aligns by characters (not bytes) and truncates over-long names/emails
  with an ellipsis so one long row cannot shear the table (full values in
  `--json`).
- `status --json` has a stable shape: every key present on every row (null
  when unknown) plus an `unreadable` flag.
- Parent directories swapdex creates for credential files (e.g. a fresh
  `~/.codex`) are 0700, not umask-default.

## [0.1.6] - 2026-07-06

### Added
- `swapdex usage [--json]`: recent local token usage per tool over the last 5h
  and 7d, summed from `~/.claude` and `~/.codex` session logs. A machine-wide
  activity gauge (not tagged by account, not the billed quota) so you can tell
  when to switch. Reads local files only - never the network, keeping the
  switcher-not-rotator stance intact.
- `use` now warns (best-effort) when the tool being switched has a live session
  running: a running session holds the old token and can overwrite the login
  you just switched to on its next refresh, so it prints a note to restart it.
  Detection is an exact process-name match (never a false alarm from a stray
  path), local only.

## [0.1.5] - 2026-07-06

### Changed
- `swapdex login --tool claude` now drives the flow instead of only printing
  guidance: if you are not logged in it opens Claude Code so you can sign in and
  auto-captures the result; if you already are, it guides the add/switch step.
  (Claude Code has no login subcommand, so a re-login to a different account is
  done inside the app.)

## [0.1.4] - 2026-07-06

### Added
- Guided onboarding: `swapdex setup` (interactive wizard - saves the accounts
  you are logged into, offers to add more, shows how to switch) and
  `swapdex login <name>` (log in and save in one step). The empty state and the
  no-argument banner now point new users to `swapdex setup`.

### Fixed
- `login`/`setup` back up the current Codex login before running `codex login`
  (which deletes `~/.codex/auth.json`), so an interrupted login is never lost.

## [0.1.3] - 2026-07-04

### Fixed
- `ls`, `status`, and the MCP `list_accounts` track the active account per tool
  (`active_tools`), fixing a mixed cross-tool state that marked both profiles
  active with a bare `*`.
- Removed the dead `active.json` hint - the live login drives every marker, so
  this dropped per-switch fsync churn and a corrupt-file surface.
- `ls` uses two-pass column widths and falls back to the tier when an email is
  missing (no stray leading-space `[tier]`).
- `session_link` skips the sessionwiki shell-out under `SWAPDEX_ROOT` so an
  isolated run never reads the host's real sessions.

## [0.1.2] - 2026-07-03

### Fixed
- `--tool` is a strict value set: a typo (`--tool cluade`) is rejected with the
  possible values instead of silently falling through to both tools.
- `use` no longer reports "already active" when the account id is empty, which
  could have kept the wrong account.
- `ls`/`status` inspect all of a profile's tools; Codex identity and the
  stale/expired marker were previously hidden behind the alphabetically-first
  `claude-code`.
- `add` (default) attaches a newly-available tool without forcing `--update`.

### Added
- The npm package ships its README (was blank on npmjs).

## [0.1.1] - 2026-07-03

### Added
- Shell completions: `swapdex completions <bash|zsh|fish|...>`.
- `status --json` for scripting.
- `ls` marks a saved login `(expired)` (Claude) or `(stale)` (Codex) so you know
  to re-capture it.

## [0.1.0] - 2026-07-03

### Added
- Initial release. Switch between multiple Claude Code and Codex login accounts
  locally: `add`, `use`, `ls`, `status`, `rm`, `rename`, `sessions`, and a
  read-only `mcp` server. In-place credential file swap, hardened for safety
  (0600 files, back-up-then-apply, symlink/root refusal, atomic writes, and a
  build-enforced no-network guarantee). Distributed via crates.io, Homebrew,
  npm, and prebuilt release binaries.
