# swapdex Permanent-Slot Account Model — Design

**Status:** Draft for review
**Date:** 2026-07-14
**Author:** youdie006 (with cross-model design review: codex + ChatGPT Pro)

## 1. Overview

Move swapdex from a **credential-copying switcher** to a **permanent-slot
model**: each account lives in its own stable `CLAUDE_CONFIG_DIR` (its own
Keychain item), and "switching" never copies a token. This eliminates the
account-logout class entirely, while keeping the everyday surface
(`add` / `use` / `ls`) familiar so users never have to learn the machinery.

## 2. Problem

swapdex today copies the live OAuth token between accounts through one shared
Keychain slot (service `Claude Code-credentials`, keyed by `CLAUDE_CONFIG_DIR`).
Claude's refresh tokens **rotate** (each refresh revokes the previous one). When
a `claude` session keeps running after a switch, its next refresh rewrites the
slot's token and revokes the snapshot swapdex just saved for the outgoing
account — so switching back later logs that account out.

This was confirmed on a real multi-`CLAUDE_CONFIG_DIR` macOS machine: accounts
that lived in their own config dirs (via `claude-work` aliases) never logged
out, while accounts that swapdex copy-swapped through the bare slot did. Two
independent design reviews (codex, ChatGPT Pro) concluded the same: **with a
shared-slot + snapshot architecture, absolute robustness is impossible**; only
per-account permanent slots eliminate the credential movement that goes stale.

The 0.25.0 running-session guard *prevents* the dangerous switch but does not
*eliminate* the class (a session can start right after the guard check). This
design removes the class.

## 3. Design principles

1. **No credential copying, ever.** Each account refreshes in place in its own
   Keychain item. There is no snapshot to go stale.
2. **The machinery is invisible.** Users keep the mental model "swapdex holds my
   accounts; I switch between them." They never need to know the words "slot",
   "CONFIG_DIR", or "pointer".
3. **Onboarding is the product.** With a model shift, adoption and correct use
   are won or lost at first run. Onboarding is a first-class, state-aware
   component — not a doc.
4. **Serve both personas on one foundation.** Casual users (one or two accounts)
   and power users (many concurrent accounts) both sit on the same permanent-slot
   base; only the convenience layer differs.
5. **Explain the win, not the mechanism.** "Your accounts stay separate and never
   get logged out" — never "permanent `CLAUDE_CONFIG_DIR` slots".

## 4. Architecture

Each account = a **permanent slot** = a stable, absolute `CLAUDE_CONFIG_DIR`.
The token lives in that slot's own Keychain item (`Claude Code-credentials-<hash
of the dir>`) and rotates there in place. Two entry points sit on this base:

- **`swapdex run <account> [-- args]`** — launch `claude` pointed at the
  account's slot (`CLAUDE_CONFIG_DIR=<slot> exec claude args`). Concurrent-safe:
  each terminal picks its own account. This is the power/multi-account path.
- **`swapdex use <account>`** — quietly repoint the "default account". An opt-in
  `claude` **shim** on `PATH` reads that pointer and launches the real `claude`
  in the default account's slot, so a plain `claude` command follows switches.
  This is the casual path and mirrors today's `use` UX (switch only; launching
  is separate).

Neither path copies a credential.

## 5. Onboarding (centerpiece)

First run (`swapdex` with no subcommand, or a first `use`/`add`) detects state
and guides from there. One decision at a time, safe default is `[Y]`, copy
explains the win. Three entry states:

### 5.1 New user — no accounts saved, but logged into Claude
```
Claude is signed in as alice@work.
  Save it as your first account? name [work]: ⏎
✓ Saved 'work'. Add another with: swapdex add <name>
  Make plain `claude` follow your switches? [Y/n] ⏎   ← installs the shim once
```

### 5.2 Existing swapdex user — has copy-model profiles (upgrade)
```
swapdex now keeps each account in its own space — the surprise logouts when
switching are gone.
  Move your 3 saved accounts over? [Y/n] ⏎
  → per account: if the login is live, adopt it; if expired, ask to re-log in
✓ Migrated. (The old copy-switch is retired; the 0.25.0 guard protects you until
  migration finishes.)
```

### 5.3 Existing CONFIG_DIR-alias user — has `~/.claude-*` dirs
```
Found 2 Claude config dirs (~/.claude-company, ~/.claude-company2).
  Register them as swapdex accounts? [Y/n] ⏎   ← adopted in place, not moved
✓ Registered 'company', 'company2'. Use them as before; now with list / switch /
  sessions / quota on top.
```

### 5.4 Ongoing onboarding via `doctor`
Onboarding does not "end". `swapdex doctor` continuously detects half-configured
states — shim missing or shadowed on `PATH`, a broken shared-config symlink, a
slot whose login expired — and prints the single next step.

**Outcome:** a user reaches a safe, working state through a few `[Y/n]` prompts
with zero exposure to the word "slot". The only thing they notice is that it
stopped breaking.

## 6. Slot layout & config isolation

### 6.1 Disk layout
- **Managed slots:** `<data_dir>/swapdex/slots/<slot-id>/`, where `<slot-id>` is
  a stable identifier that does NOT change when the account is renamed. Store dir
  is `~/.local/share/swapdex` (Linux) / `~/Library/Application Support/swapdex`
  (macOS).
- **Name → slot mapping:** a registry (e.g. `slots.json`) maps the human name to
  `{ slot_id, config_dir (absolute), adopted: bool }`. Renaming changes the name,
  never the `config_dir` — so the Keychain hash stays stable (a hard requirement:
  the Keychain service is derived from the raw `CLAUDE_CONFIG_DIR` string).
- **Adopted slots:** for existing `CLAUDE_CONFIG_DIR` dirs (e.g.
  `~/.claude-company`), the registry records the existing absolute path as the
  slot's `config_dir`. The directory is NOT moved.
- **Path hygiene:** `config_dir` is always an absolute, canonical path — no `~`,
  no symlinks in the path itself, no trailing-slash variance. Different spellings
  hash to different Keychain services, so the stored value is the single source
  of truth.

### 6.2 What is per-account vs shared
Per the chosen model — **global config shared, auth + history per account**:

- **Per-slot (real files in the slot):** the credential file fallback
  (`.credentials.json`), conversation history / `projects`, and the account
  identity (`oauthAccount`). The Keychain token is inherently per-slot (keyed by
  the dir).
- **Shared (linked from a single source):** `settings.json`, global `CLAUDE.md`,
  `plugins/`, and MCP server config — so switching accounts never changes the
  user's tooling.

### 6.3 The `.claude.json` mixed-file problem (OPEN — see §14)
`.claude.json` mixes per-account data (`oauthAccount`) with shared-ish data
(`mcpServers`, `projects`, `theme`) in one file, so it cannot simply be
symlinked. Recommended default (to verify against Claude's actual config
loading): keep `.claude.json` per-slot (it carries the account identity), and on
slot creation **seed** the shared `mcpServers` into the new slot from a shared
source, with a `swapdex sync-config` command to re-propagate later. Confirm
whether Claude reads MCP from a separate file that CAN be symlinked before
finalizing.

## 7. Commands & UX

Familiar surface unchanged; slot machinery hidden.

- **`swapdex add <name>`** — create a slot and run the tool's own first-login in
  it (the slot's Keychain item is created by Claude at sign-in, never faked).
- **`swapdex use <name>`** — atomically repoint the default-account pointer.
  Quiet (mirrors today's switch UX). On the first ever `use`, offer to install
  the shim once.
- **`swapdex run <name> [-- args]`** — launch `claude` in that account's slot
  immediately (concurrent). Advanced; casual users never need it.
- **`swapdex ls` / `status` / `sessions` / `quota` / `doctor`** — all slot-aware:
  they read each account's own token and history from its slot.
- **Deletion (kept explicit, per review):** `swapdex rm <name>` removes the
  name→slot mapping and the managed slot directory (local only) by default;
  removing the Keychain credential and/or a server-side logout are separate,
  explicit actions (e.g. `--forget-login`). An adopted (not swapdex-created) dir
  is unregistered, never deleted, unless explicitly requested.

## 8. The shim & default pointer

- **Pointer:** `<data_dir>/swapdex/active-claude` holds the default account's
  slot path, written atomically by `use`.
- **Shim:** a small executable `claude` (installed to a dir the user puts ahead
  of the real `claude` on `PATH`, e.g. `~/.local/bin/claude`) that reads the
  pointer, exports `CLAUDE_CONFIG_DIR=<slot>`, and `exec`s the real `claude`.
- **Why a shim is unavoidable:** `swapdex use` cannot mutate its parent shell's
  environment, so *something* must intercept a plain `claude` invocation. The
  shim (or an equivalent shell hook) is the minimal, honest mechanism. The design
  makes it a one-time `[Y]` during onboarding.
- **`doctor` guards it:** verifies the shim exists, is ahead of the real binary
  on `PATH`, and points at a valid pointer; warns on bypass/shadowing.

## 9. Migration

`swapdex migrate` (also offered inline during onboarding state 5.2/5.3):

1. **Discover** old copy-model profiles and existing `CLAUDE_CONFIG_DIR` dirs.
2. **Adopt** existing config dirs in place (register, don't move).
3. **For each copy-model profile:** create a slot; **verify the saved snapshot's
   live account identity**; if verified-current, import the token into the slot's
   Keychain once; if stale/expired, prompt a fresh re-login into the slot (never
   import an unverifiable snapshot).
4. **Offer the shim.**
5. After migration, `use` on a migrated account = pointer repoint; legacy
   copy-switch is retired for that account.

Migration should ask the user to stop running `claude` sessions first (a running
session complicates verification and login).

## 10. Coexistence & the 0.25.0 guard

- The permanent-slot model is introduced additively; existing installs keep
  working. `use` behaves by target: a **migrated slot** → repoint the pointer; a
  **legacy snapshot** → the old (0.25.0-guarded) copy-switch, with a nudge to
  migrate.
- The **0.25.0 running-session guard stays as the safety net** for any remaining
  legacy copy-switch path during the transition.
- A later release removes the legacy copy-switch code (and with it the guard's
  reason to exist) once migration is the norm.

## 11. Error handling & edge cases

- **Slot path stability:** renames never touch `config_dir`; the Keychain hash is
  therefore stable across renames.
- **Adopted-dir safety:** swapdex never deletes a directory it did not create.
- **Broken shared-config symlink / missing shim / expired slot login:** surfaced
  by `doctor` with the next concrete step.
- **Migration with live sessions:** advise stopping sessions; do not import a
  snapshot whose live identity cannot be verified.
- **`SWAPDEX_ROOT` sandbox:** slot creation, adoption, pointer, and `run` all
  operate under the sandbox root in tests; no real Keychain or real `claude` is
  touched.

## 12. Testing strategy

- **Pure/unit:** pointer read/write, slot-id ↔ path ↔ name mapping, path-hygiene
  normalization, shared-config link planning, migration verify/import decision
  logic.
- **Sandbox integration (`SWAPDEX_ROOT`):** `add` creates a slot; `use` repoints
  the pointer (no copy); `run` launches a fake `claude` with the right
  `CLAUDE_CONFIG_DIR`; adoption registers an existing dir without moving it;
  migration imports a verified snapshot and defers a stale one to re-login.
- **Shim:** unit-test the shim script's pointer resolution and `CLAUDE_CONFIG_DIR`
  export; `doctor` PATH-ordering detection.
- **macOS Keychain paths** are verified on a Mac (as with the 0.25.0 guard's
  `ps eww` path), since the sandbox is file-only.

## 13. Implementation phasing

This design is large; it is delivered in phases, each a separately shippable,
testable increment (each gets its own implementation plan):

- **Phase 1 — Slot foundation:** slot registry + creation, `add` into a slot,
  `swapdex run <name>`, `ls`/`status` slot-aware, sandbox tests. Delivers the
  no-copy path end-to-end for power users.
- **Phase 2 — Default pointer + shim + `use` repoint:** the casual path; shim
  install offer; `doctor` shim checks; shared-config linking.
- **Phase 3 — Migration + legacy retirement:** `migrate` wizard, adoption of
  existing dirs, verified import vs re-login, remove legacy copy-switch.

The first implementation plan covers **Phase 1**.

## 14. Open questions (with recommended defaults)

1. **MCP / shared-config mechanism for `.claude.json`** (§6.3): the file mixes
   per-account and shared data. *Default:* keep `.claude.json` per-slot; seed
   shared `mcpServers` on slot creation + a `sync-config` command. *To confirm:*
   whether Claude reads MCP from a separate, symlink-able file.
2. **Slot-id scheme:** UUID vs a slugified stable id. *Default:* an opaque stable
   id decoupled from the display name, so rename never changes the path.
3. **Shim install location & PATH guidance:** `~/.local/bin` vs a swapdex-owned
   bin dir the user adds to `PATH`. *Default:* a swapdex-owned bin dir, with
   `doctor` verifying PATH order — avoids clobbering an existing `~/.local/bin`
   entry.
4. **Deletion granularity** (§7): how many separate delete actions to expose in
   v1. *Default:* `rm` (mapping + managed dir) plus `--forget-login` for the
   Keychain credential; server-side logout stays a manual `claude` action.
