# Slot Freshness + Handoff on the Permanent-Slot Base — Design

**Status:** Draft for review
**Date:** 2026-07-22
**Author:** youdie006

## 1. What this adds

This builds on
[`2026-07-14-swapdex-permanent-slots-design.md`](./2026-07-14-swapdex-permanent-slots-design.md),
whose foundation is already partly implemented (`src/slots.rs`: the registry,
`create`/`adopt`/`set_default`/`default_dir`; `swapdex run <name>`; the
`active-claude` pointer). That design proves the slot model removes the
credential-copy that goes stale. It leaves two things open that this spec fills,
plus one unrelated quick fix:

1. **Idle-slot freshness.** The slot model kills *rotation-collision* staleness,
   but an account left unused can still let its refresh token reach a server-side
   absolute expiry. This spec relies on Claude's own in-slot refresh and adds
   read-only staleness detection in `doctor` — swapdex never mints tokens itself
   (see §3 for why minting would re-create the logout risk).
2. **Handoff onto the slot base.** Unifies the separate account-handoff feature
   (continue the same conversation on a fresher account) as "UX 2" sitting on the
   slot foundation, with sessionwiki carrying the transcript across the slot
   isolation boundary.
3. **`r` (restore) fix.** Independent of the slot work; ship first.

## 2. Why freshness still needs a small layer

The 2026-07-14 design is right that a slot never goes stale from *copying*: each
account's token lives in its own slot and Claude rotates it in place. Two residual
facts remain:

- An **idle** slot is refreshed by no one. Its access token expires in hours; its
  refresh token survives — until a possible server-side **absolute** expiry (its
  exact lifetime is unknown to us).
- The real, observed pain ("자꾸 만료되네") was rotation-collision in the classic
  model, which slots already remove. So this layer is a **safety margin for
  long-idle accounts**, not the main fix.

Because of that, the answer is deliberately light: keep everyday accounts fresh as
a side effect of normal use, and accept that a slot left untouched for weeks may
need one re-login (surfaced by `doctor`, per the base spec).

## 3. Freshness design (Claude's on-run refresh + read-only doctor detection)

swapdex does **not** mint tokens. It has no OAuth client today (its one network
read, `quota`, shells out to `curl`), and implementing the refresh dance itself
would re-create the exact risk the slot model removes: a botched refresh (wrong
client/params, or a race) burns the refresh token and logs the account out. It is
also unnecessary — Claude Code already refreshes a slot's token correctly, in
place, every time it runs.

So freshness is two read-only pieces:

- **Claude's on-run refresh (the actual mechanism).** Every `swapdex run <name>`
  (or a plain `claude` via the shim) launches Claude in that slot; Claude refreshes
  the slot's own token in place. Any regularly-used account therefore never goes
  stale — nothing to build.
- **doctor staleness detection (the safety net).** `swapdex doctor` reads each
  slot's last-refresh signal — the `expiresAt` in its `.credentials.json`, or on
  macOS (Keychain-stored login) the item's modification date via an
  attribute-only lookup (no secret read, no ACL prompt) — taking the newest when
  both exist (a pre-Keychain-era leftover file must not shadow a fresh Keychain
  item). No network, no write. For a slot with no login it prints "run once to
  sign in"; for one idle past the stale window, "run once to refresh, re-login
  if it asks." This is the only new code, and it only reads.

This covers the common case (used accounts stay fresh automatically) and surfaces
the rare one (a slot idle past its refresh-token lifetime) without swapdex ever
touching the token or the network beyond reading a local timestamp.

### 3.1 If long-idle expiry proves to be a real, frequent problem

Only then revisit an active refresh, in order of safety: (a) trigger Claude's own
refresh (launch it briefly in the slot) rather than swapdex minting; (b) a
swapdex-owned OAuth refresh as a last resort, gated so it only ever runs on an
**idle** slot (the base design's running-session check) and never races a live
session. Both are deferred — YAGNI until observed.

## 4. Handoff on the slot base (UX 2)

Absorbs the account-handoff feature (sessionwiki side:
`sessiondex/docs/superpowers/specs/2026-07-22-account-handoff-design.md`). Framed
on slots:

- **Goal.** When account A hits its usage limit, continue the **same
  conversation** on account B with one command.
- **Why slots make it clean.** Each slot has its own `projects/` transcripts. B
  cannot see A's transcript, so sessionwiki `migrate` carries it into B's slot —
  the isolation boundary is exactly where sessionwiki earns its place. Then
  `claude --resume <uuid>` re-sends the local context to the API billed to B.
- **`swapdex continue`.** Identify the current project's latest session → pick B =
  the same-tool slot with the most remaining quota (`quota`/`usage`) or explicit
  `--account` → carry the transcript into B's slot (sessionwiki) → print/exec
  `claude --resume`.
- **Concurrency note.** A running session cannot be swapped under itself, so this
  is stop-then-resume, not a live splice. "You hit the wall, so you are stopping
  anyway."

### 4.1 Feasibility gate (verify before building)

Does `claude --resume` continue on a **different** account, or is the session
bound server-side to its creating org? rnd and bsgong are the **same org**
(polarisai.co.kr), so within-org handoff is expected to work; cross-org is the
uncertain case and falls back to sessionwiki `brief` (a fresh session on B seeded
with a markdown summary). Confirm with a real two-account resume test **before**
building `continue`.

## 5. `r` (restore) fix — independent, ship first

Today the TUI `r` runs `restore` — "put back the login that was live before the
last switch" (`commands.rs` `restore`, backed by `store.rs` `load_backup`, newest
of 2 backups). In hub-and-spoke use (always switching away from one base account),
"the login before the last switch" is always that base, so `r` appears to always
return to one fixed account — not the account the user actually used before.

**Fix.** Remap the TUI `r` key to the **previous-account toggle** (`use -`, which
already toggles to the previously-used profile — `commands.rs:184,478`). `restore`
remains available as an explicit command / safety net; it is simply not what `r`
should do. This is a TUI key-binding change plus its test; it does not touch the
slot work.

## 6. Build order

1. **`r` fix** (§5) — done.
2. **Migration to slots** — the actual pain-fix. Get the existing accounts onto
   slots (the 2026-07-14 spec's onboarding/migration, Phase 2/3). This removes the
   rotation-collision expiry the user hits; do it before any freshness work.
3. **doctor staleness detection** (§3) — read-only. Small; ship after migration so
   it has real slots to check.
4. **Handoff feasibility test** (§4.1), then **`swapdex continue`** (§4) — gated on
   the test.

Onboarding-defaults-to-slot and the classic→slot migration itself are specified in
the 2026-07-14 spec's Phase 2/3; this spec only reprioritizes them ahead of
freshness and points `doctor` at the residual staleness.

## 7. Scope

- **v1 is claude-first** — the pain is all Claude accounts (rnd/bsgong). codex and
  gemini stay on the classic model; extending slots to them is future work.
- **Classic coexists** — per the base spec, slot and classic profiles live side by
  side; moving an account to a slot is per-account and reversible.

## 8. Testing

- **Freshness (doctor):** unit-test the read-only staleness decision from a slot's
  `expiresAt` (fresh / expired-recently / expired-long-past-threshold) and that
  doctor emits the "run once / re-login" step only for the last case. No network,
  no token write.
- **`r` fix:** the TUI key handler maps `r` to the previous-toggle action, not
  restore.
- **Handoff:** the feasibility test is manual (two real accounts). `continue`'s
  B-selection (most-remaining-quota) and transcript-carry are unit/sandbox tested;
  the resume exec is asserted by the printed/exec'd command shape.
