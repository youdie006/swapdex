# Codex session discovery and resume repair

## Problem and evidence

Swapdex launches new Codex sessions with an ephemeral `swapdex` or
`swapdex-<account>` provider, but removes that provider definition on `resume`.
Codex 0.154.0 filters its local picker by provider and reads the recorded
provider when resuming. Existing conversations disappear from the picker and
direct resume fails with `Model provider 'swapdex' not found`.

The shared rollout files and paginated conversation items remain intact.
Explicitly overriding the provider to `openai` successfully restores the
original history. Deleting databases or replacing the paginated history is
unnecessary. State-only provider changes are insufficient: native backfill
derives the provider from the rollout's first `session_meta` record.

## Intended behavior

- Ordinary launch, `resume`, `resume --all`, `resume --last`, explicit session
  IDs and the in-session picker use Codex's native local behavior.
- Proxy routing uses Codex's built-in `openai` provider and its supported
  `openai_base_url` override. Account selection does not become session identity.
- Login/logout bypass proxy routing. Literal prompt text such as `resume` or
  `login` must not disable routing for `exec` or an interactive prompt.
- Explicit provider/profile/remote choices remain authoritative.
- Codex WebSocket probes receive HTTP 426 locally and immediately fall back to
  the existing HTTP Responses transport without contacting an upstream account.
- Historical Swapdex-generated providers are repaired to `openai`, with a
  durable private journal and matching updates to existing state indexes.
  Conversation records, paginated history databases, IDs, paths, titles and
  ordering timestamps are preserved.

## Repair boundaries

Discover the active Codex home, the bare Codex home and registered Codex slot
homes through `Paths`/`Slots`; deduplicate canonical paths. Read only rollout
metadata in their sessions and archived-session roots. Never inspect auth or
other tools' histories. Only exact `swapdex` and the old sanitized
`swapdex-<account>` namespace are eligible; unrelated providers remain intact.

Back up each original provider token and the identity/context needed to
validate it before changing anything. Use a fixed-width, in-place replacement
of the JSON string token with `"openai"` plus JSON whitespace. This preserves
the inode, every subsequent byte offset, and writes from already-open append
handles. Do not truncate or atomically replace an actively appended rollout.
Flush the journal before the token and recover interrupted token writes on a
later invocation only when the saved surrounding header still matches.

Serialize repairs with a store lock. Apply conditional SQLite transactions to
existing compatible state databases, only for IDs whose scanned rollout now
has the repaired provider. Preserve all other columns and tables. Do not create
missing state databases. Unknown schemas, unreadable files, lock contention and
partial repairs must be reported truthfully and remain retryable. Dry-run must
make no filesystem or database changes.

Acquire Codex's `thread-writer-locks/<id>.lock` nonblockingly in every discovered
home sharing the physical rollout; hold those guards through index updates.
Skip busy threads for a later launch. Fixed-width writes and header rechecks
remain necessary for older clients and homes outside the registry. Compressed
`.jsonl.zst` files are reported as unsupported/retryable; never patch compressed
bytes as though they were JSON. No compressed rollouts were present in the
affected machine's bare session directory at diagnosis.

## Validation and delivery

Regression tests cover mixed providers, archive roots, cross-slot indexes,
idempotency, dry-run, append-handle preservation, interrupted writes, malformed
headers, unrelated history, login and explicit-option handling, and HTTP 426.
Use a stock Codex executable with isolated fixtures to verify native listing,
resuming and proxy transport. Verify the user's two original sessions at idle
without submitting a model prompt; compare the saved conversation item hashes.

Ship the changelog, exact version and release notes with source; publish the
documented installation channels, install the exact version on the affected
WSL machine, regenerate shims and verify restarted proxies. Preserve release
and installation evidence in the repository/PR/release record.
