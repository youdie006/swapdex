# swapdex Proxy Mode — Continue the Same Session on Another Account — Design

**Status:** Draft for review
**Date:** 2026-07-27
**Author:** youdie006

## 1. Goal

When the active account runs out of quota, the **same running conversation**
continues on another account — no new chat, no manual resume, no lost context.
And at any moment the user can point the current session at a different account
of their choosing.

This is the one thing credential-swapping cannot do. `swapdex use` rewrites files
on disk, but a running `claude` holds its auth in memory, so a switch only takes
effect in the next session. Intercepting the API traffic changes that: each turn
is a fresh HTTP request, so the account can be chosen **per request**.

## 2. Prior art (and why we are not just using it)

`KarpelesLab/teamclaude` (Node) is a multi-account Claude proxy with quota-based
rotation; `jung-wan-kim/teamclaude` is a fork ~90 commits ahead that hardened it
in production. Both prove the approach works. Two reasons to build our own rather
than adopt theirs:

1. **This is the user's own harness.** swapdex already owns accounts (slots),
   identity, quota, sessions, and a TUI. Proxy mode belongs on that base, not
   beside it as a second tool with its own account store.
2. **Their credential model is the staleness bug swapdex already fixed.**
   teamclaude keeps account tokens **copied** into `~/.config/teamclaude.json`.
   That is exactly the snapshot-goes-stale class the permanent-slot model
   eliminated (see `2026-07-14-swapdex-permanent-slots-design.md`). A proxy
   reading straight from slots has **no second copy to rot**.

The fork's commit history is treated as a **map of what breaks** — its hard-won
fixes become our v2 requirements (§8) instead of bugs we rediscover.

### What we add that they do not have

- **Exact per-turn attribution.** `session_link.rs` today *infers* which account
  ran a session by joining the switch timeline with session start times —
  explicitly best-effort, with an `unattributed` bucket. The proxy **knows**
  which account served every single turn. Feeding that to sessionwiki turns
  guesswork into evidence ("this conversation: rnd 12 turns, bsgong 8 turns").
- **No credential copies** (above).
- **Quota telemetry for free.** Every upstream response carries rate-limit
  headers, so utilization is observed rather than probed — no keep-warm traffic,
  no ToS gray area, and the existing `quota`/`usage`/TUI bars become live.
- **Safety-first defaults**: turn-boundary-only switching (never a severed
  answer), loopback-only bind, no unattended automation.

## 3. Architecture

One binary, one new subcommand: **`swapdex proxy`**.

```
claude  --ANTHROPIC_BASE_URL-->  swapdex proxy (127.0.0.1:PORT)
                                   | pick account (slot)
                                   | inject that slot's token
                                   v
                                 api.anthropic.com  (HTTPS, streamed both ways)
                                   |
                                   +-- rate-limit headers -> per-slot utilization
```

**Stack: synchronous, thread-per-connection** — `tiny_http` to receive, `ureq`
(rustls) upstream. Rationale: the whole codebase is synchronous with no async
runtime; sessionwiki already serves its web UI on `tiny_http`; the load is one
human with a handful of concurrent turns. This keeps the dependency delta near
+25 crates instead of ~+80 for tokio/hyper, in a tool that handles credentials.

**Client wiring (v1):** `swapdex proxy` prints the one line to export
(`ANTHROPIC_BASE_URL=http://127.0.0.1:PORT`). Teaching the existing `claude`
shim to export it automatically when the proxy is up is a v2 convenience, not a
v1 requirement.

**Interception is base-URL only.** No MITM, no locally generated CA, no
`NODE_EXTRA_CA_CERTS`. teamclaude defaults to MITM to also catch components with
a hardcoded `api.anthropic.com`; that buys edge coverage at the cost of a local
certificate authority, which is not a trade this tool should make. Main API
traffic is what carries the conversation, and that is what base-URL mode covers.

## 4. Credentials: read from slots, never copied

- The token for account N is read from **N's own slot**: `.credentials.json` on
  Linux, or the slot's Keychain item on macOS (service = the documented
  `Claude Code-credentials-<8 hex of sha256(config dir)>`, already derived in
  `adapters/claude.rs`).
- It is held in memory as a `Secret` (zeroized) for the request only, and
  **never written into any swapdex-owned file**. The slot stays the single
  source of truth.
- Only slot-model accounts are proxy-eligible. Legacy copy-model profiles are
  not: their snapshots are the stale-token class this design refuses to inherit.

### 4.1 The refresh problem (this amends an earlier decision)

Claude access tokens live about an hour. To rotate onto an account that has not
run for hours, the proxy **must** refresh that slot's token. The
2026-07-22 freshness spec deliberately said swapdex never mints tokens, because
a botched or racing refresh burns the refresh token and logs the account out.
Proxy mode cannot honor that as written — rotation onto an idle slot is the whole
feature. So the stance changes, narrowly, with hard rules:

1. **Only the proxy refreshes, and only for a slot it is about to use.** The
   proxy is a single process and serializes that slot's token use, so swapdex
   never races itself.
2. **Never refresh a slot with a live session.** Reuse the existing
   running-session detection (the 0.25.0 guard's process check). A slot being
   used by its own `claude` is refreshed by that `claude`; touching it is the
   original collision.
3. **Write the new token back into that slot in place**, atomically, with the
   existing `atomic`/`write_secret` path. No second store, no copy.
4. **Pass the client's own refresh traffic through untouched.** The `claude`
   running in its own slot must keep managing its own token lifecycle.
5. **On refresh failure, fail that account out** (mark unusable, report the
   remedy: `swapdex run <name>` once) and pick another. Never retry-loop a
   refresh, which is how refresh tokens get burned.

### 4.2 Learn the exact protocol instead of guessing

Three things must be exact and none of them are documented: the rate-limit
header names, whether the request body carries an account identifier that must
be kept consistent with the injected token, and the precise refresh request
shape. All three are **observable from the client's own traffic through the
proxy**. Therefore v1 starts in an observe step (§7) that records shapes — never
prompt content, never token values — and rotation is implemented against those
recorded facts.

#### Observed 2026-07-27 (Linux/WSL, real `claude`, sandboxed proxy)

The client was pointed at the proxy with `ANTHROPIC_BASE_URL` while the proxy ran
under `SWAPDEX_ROOT` with a fake slot token and `SWAPDEX_UPSTREAM` aimed at a
local sink, so no real credential ever left the machine. Recorded shape only.

- **Base-URL interception works.** `claude` sent its turns to the proxy:
  `POST /v1/messages?beta=true` (the query string is part of the path and must be
  forwarded verbatim). It also probes `GET /api/hello` first as a connectivity
  check — a proxy must answer it rather than fail.
- **The body carries an account identifier**: `metadata.user_id` is present on
  every turn. So injecting account B's token while leaving A's `user_id` makes the
  request internally inconsistent — the rewrite in §9 is required, not optional.
  Other body keys seen: `model`, `messages`, `system`, `tools`, `max_tokens`,
  `thinking`, `stream` (first turn), `context_management`, `output_config`.
- **Request headers** include `authorization`, `anthropic-version`,
  `anthropic-beta`, `anthropic-dangerous-direct-browser-access`, `x-app`,
  `x-claude-code-session-id`, and the `x-stainless-*` SDK set. Only hop-by-hop
  headers are dropped; the rest pass through untouched.
- **Streaming matters**: `stream: true` on the conversation turn, so the response
  is SSE and the chunked/streamed response path is the normal path, not an edge.

Still unobserved, so still gated:

- **The real rate-limit header names.** The sink faked
  `anthropic-ratelimit-unified-*`, which exercises the parser but does not confirm
  the names. The parser therefore matches by prefix rather than an exact list, and
  the names get confirmed by one real turn through the proxy.
- **The refresh request shape.** No refresh happened during the observation, so
  §4.1's `refresh_slot` stays gated: it is implemented only once the client's own
  refresh has been seen.

## 5. Choosing the account

- **Utilization** per slot comes from `anthropic-ratelimit-unified-*` response
  headers, kept in memory (plus what `quota.rs` already knows).
- **Switch when** the active slot is at/over the threshold (default 98%,
  configurable) or the upstream rejects a request as quota-exhausted.
- **Turn boundary only.** A request already streaming finishes on the account
  that started it. The *next* request uses the new account. No half answers.
- **Stickiness is a feature, not laziness.** Prompt caching is
  organization-scoped, so switching accounts drops the cache and the first turn
  on the new account costs more. Therefore: do not rotate for small margins;
  rotate on rejection or threshold only.
- **v1 selection rule:** most remaining quota among eligible slots. (The fork's
  "soonest session reset first" is a better rule under sustained load; it is a
  v2 refinement, recorded here so it is not rediscovered.)
- **Explicit user choice always wins.** `swapdex proxy --account <name>`, and a
  runtime pin (§6) so the user can move the current session onto a specific
  account on demand — the second half of the goal in §1.

## 6. Commands

- **`swapdex proxy [--port N] [--account <name>] [--threshold 0.98]`** — run the
  proxy in the foreground; prints the `ANTHROPIC_BASE_URL` line and a live log
  of turns (time, account, status, utilization).
- **`swapdex proxy --status`** — is a proxy running, on which port, which account
  is active, per-slot utilization.
- **Runtime pin** — point the running proxy at a chosen account without
  restarting it (so the current conversation moves accounts on demand). v1
  mechanism: a pointer file the proxy re-reads per request, in the same idiom as
  `active-claude` (`swapdex use <name>` writes it; the proxy honors it as an
  override until quota forces a rotation). No new IPC machinery.

## 7. v1 scope: prove the mechanism

1. **Feasibility check first.** Confirm `claude` on a subscription (OAuth) login
   actually honors `ANTHROPIC_BASE_URL` and reaches a local plain-HTTP endpoint.
   Everything downstream depends on this; if it does not hold, stop and
   reconsider before writing the proxy.
2. **Observe step.** Minimal pass-through proxy using only the currently active
   slot's token: forward request, stream response, and record protocol shapes
   (header names, presence/location of any account identifier, refresh request
   shape). Never log prompt content or token values.
3. **Inject step.** Choose the slot (pinned or active), read its token, inject
   `Authorization`, keep the body self-consistent with that credential, stream
   both directions, handle client disconnect by dropping the upstream read.
4. **Rotate step.** Parse utilization from response headers; on threshold or
   quota rejection, switch account at the next turn boundary (refreshing that
   slot's token first, under §4.1's rules). Log the switch.
5. **Verify the actual goal**: with a conversation running, exhaust or pin away
   from the active account and confirm the same conversation continues on
   another account, billed to it, with no new chat.

Out of scope for v1: hardening (§8), MITM mode, shim auto-wiring, session
attribution export, launchd/systemd supervision, non-Claude tools.

## 8. v2 hardening (requirements mined from the fork)

Each item below is a bug the fork already hit; they are requirements, not ideas.

- **Classify 429s** — quota-exhausted (rotate) vs per-minute throttle (wait and
  retry the same account) vs concurrency (spill to another). Conflating them
  produces 429 storms.
- **Warm up unmeasured accounts** before trusting them in rotation; make the
  warm-up concurrency-safe.
- **Per-account concurrency caps**, and track in-flight slots by account handle
  (not by index — a removed account invalidates indices).
- **Connection affinity** for prompt-cache locality, deferred until an account
  is measured, and never rewritten during a transient failover.
- **Bound retries** so a throttled account cannot loop forever.
- **Retry 529/5xx** so a transient upstream error does not fail the user's turn.
- **Abort the upstream read on client disconnect** (otherwise streaming capacity
  leaks).
- **401 → fail the account out**, with the remedy printed.
- **Guard token-refresh persistence** against an account removed mid-flight.

## 9. Safety, privacy, ToS

- **Loopback only.** Bind 127.0.0.1 and refuse any other bind.
- **No prompt content in logs, ever.** Log time, account name, status, and
  utilization. Never a token, never a body.
- **No keep-warm, no quota probing.** Response headers already carry
  utilization, so the proxy never originates traffic of its own.
- **Human in the loop.** The proxy serves an interactive `claude`; it does not
  generate turns. Unattended automation is out of scope by design.
- **Genuine Claude Code CLI, your own subscriptions.** Anthropic restricts
  third-party frontends on subscription OAuth; this proxies the real CLI's own
  traffic for accounts the user owns. Body rewriting is limited to keeping a
  request consistent with the credential actually serving it — not
  impersonating another account. Concretely (per §4.2's observation): when the
  injected token belongs to a different account than the one the client wrote
  into `metadata.user_id`, that field is rewritten to the serving account's own
  identity. Nothing else in the body is touched, and the prompt never is.
- Refuse to run as root (existing `ensure_not_root`), and keep the store's 0700
  hygiene.
- The proxy is **opt-in**: nothing changes for users who never run it.

## 10. Platform support: Linux, WSL, and macOS

All three are first-class. They differ in exactly one place — where a slot's
credential lives — and the codebase already draws that line.

- **Linux / WSL** — Claude keeps the login in a file inside the config dir, so a
  slot's token is a plain read of `<slot>/.credentials.json`. WSL is Linux for
  this purpose: same paths, same code, no Keychain.
- **macOS** — the login lives in the Keychain under the service derived from the
  config dir. The proxy reads it through the same absolute-path `/usr/bin/security`
  mechanism `adapters/claude.rs` already uses (which is why no ACL prompt
  appears: Claude created the item trusting that binary). The one addition is a
  read variant that takes an explicit service name, so a *slot's* item can be
  read rather than only the environment-derived one.
- **Windows native is out of scope**, consistent with the project's existing
  Unix-only stance. WSL is the supported way to run it on a Windows machine.

Portability constraints this puts on the stack:

- **TLS via rustls with bundled roots** (`webpki-roots`), not native-tls. No
  OpenSSL to find, no per-distro system-trust differences, identical behavior on
  all three. This is a portability requirement, not a preference.
- **Loopback only, one side of the boundary.** `claude` and the proxy must run in
  the *same* environment. Inside WSL2 the proxy's `127.0.0.1` lives in WSL's own
  network namespace, so a `claude` running on Windows cannot reach a proxy inside
  WSL (and vice versa). v1 states this as a requirement rather than bridging it;
  `--status` should say which environment it is bound in so a cross-boundary
  mistake is obvious instead of mysterious.
- **No platform-specific process supervision in v1.** The proxy runs in the
  foreground on all three; launchd/systemd wrappers are v2 and per-platform.

## 11. Testing

- **Hermetic integration**: a local fake upstream plus fake slot credentials
  under `SWAPDEX_ROOT`. Assert token injection, streaming pass-through both ways,
  utilization parsed from headers, rotation at a turn boundary (not mid-stream),
  client-disconnect abort, and that no credential is written outside its slot.
- **Pure unit**: header/utilization parsing, the rotation decision table
  (fresh / at threshold / quota-rejected / throttled / concurrency), slot
  selection, and the refresh-eligibility rules from §4.1 (idle vs live session).
- **No real network in any test**, matching how `quota.rs` is tested today.
- **Per-platform**: the hermetic suite runs on Linux/WSL and macOS alike; the
  macOS Keychain read path is the one part the sandbox cannot cover (it is
  file-only by design), so it is verified on the Mac the same way the 0.25.0
  guard's `ps eww` path was.

## 12. Relationship to existing specs

- Builds on `2026-07-14-swapdex-permanent-slots-design.md` (slots are the
  credential source; only slot accounts are proxy-eligible).
- **Amends** `2026-07-22-slot-freshness-and-handoff-design.md` §3: swapdex does
  mint tokens, but only inside proxy mode and only under §4.1's rules. The
  read-only `doctor` staleness check stays as is.
- **Supersedes handoff-by-resume as the primary answer** to the quota wall.
  `swapdex continue` (stop, switch, `claude --resume`) remains the fallback for
  anyone not running the proxy, and for cross-org cases.
