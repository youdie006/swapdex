# Usage Endpoint Backoff Implementation Plan

> **For agentic workers:** REQUIRED: Use superpowers:subagent-driven-development
> for implementation and review. Only the main agent commits or pushes.

**Goal:** Stop repeated Claude usage requests while the provider is limiting
lookups, including across picker restarts and concurrent proxy/quota callers.

**Architecture:** One persisted per-credential retry deadline, protected by a
file lock, wraps a single header-aware usage request. Preserve existing quota
results and last-success timestamps. Keep the change outside authentication
and model-request routing.

**Tech Stack:** Rust; existing fs2, tempfile, serde and sha2; the already-resolved
httpdate parser; isolated Python/PTY checks and fake curl integration fixtures.

## Chunk 1: Regression and bounded implementation

**Files:**
- Add `src/quota_backoff.rs` and its unit tests; register it in `src/lib.rs`.
- Update `src/quota.rs` for header capture and persisted throttled lookups.
- Update `src/commands.rs` and `src/proxy/mod.rs` to pass their `Paths`.
- Add `tests/quota_backoff.rs` for real command processes with fake curl.
- Update `Cargo.toml`/`Cargo.lock` for a direct `httpdate` dependency.

- [x] Run `cargo test --locked --lib quota` on the unchanged base and retain its
  result. Run two installed `quota --json` commands with a synthetic 429 and
  confirm the old implementation makes eight HTTP attempts.
- [x] Add a regression that invokes `quota --json` twice against one fixture
  root and asserts exactly one fake HTTP request, two throttled statuses and
  byte-identical credentials. Run `cargo test --test quota_backoff` and observe
  the expected assertion failure before implementation. Set both
  `SWAPDEX_ROOT` and `SWAPDEX_CURL`; no test may use a real credential/network.
- [x] Implement the per-key coordinator with private paths, stable file lock,
  bounded lock acquisition, atomic state writes and explicit invalid-state
  behavior. Store exactly the spec's v1 failures/throttled_at/retry_at schema,
  validate bounded counts and timestamp differences, and atomically replace
  records. On non-429 HTTP remove the record while retaining the lock file;
  transport failure retains it. Test with supplied times, without long sleeps.
- [x] Capture final response headers for quota alone. Parse numeric/HTTP-date
  Retry-After, exercise multiple blocks and mixed header case, and verify the
  zero/invalid/date-in-past fallback and 24-hour upper bound.
- [x] Route quota command and proxy measurements through the same coordinator.
  Replace the old loop in `fetch_with_retry`, change `fetch_many` and proxy
  measurement to pass `Paths`, and invoke raw quota HTTP only once per granted
  attempt. The shared `run_curl_cfg` body/status API remains untouched.
  Existing HTTP/transport classifications stay intact. Add
  `Fetch::Coordination(String)` and JSON `unavailable`/detail for local state
  failures; render that reason in human/TUI output without setting global
  offline. One 429 means one outbound attempt; it never invokes renewal,
  switches accounts or changes credentials.
- [x] Add concurrent-process exclusion, expiry/recovery, repeated-429
  escalation, independent credentials, success reset, 401/403/malformed-2xx/
  other-HTTP resets, transport-error count retention, and lock/state failure.
  Verify private state and credential secrecy. Assert failed/deferred reads
  preserve cache figures and `at`; a fresh success updates them using the
  existing observation-time convention. Verify the same regression is now
  green, then run `cargo test --test quota_renewal --test quota_identity
  --test quota_cache_concurrency --test quota_backoff`.

## Chunk 2: Review, release and installation

**Files:**
- `CHANGELOG.md` and version metadata following the existing release workflow.
- `docs/research/2026-09-17-usage-endpoint-backoff.md`.
- Version-specific GitHub release/PR records, npm/crate/tap publication records.

- [ ] Perform a read-only spec-compliance review, resolve findings, then a
  separate code-quality review. Cite exact files and executed checks.
- [x] Run `cargo test --all --locked`, `cargo clippy --all-targets -- -D warnings`,
  `cargo fmt --all -- --check`, `git diff --check`, the CI dependency/runtime
  gates, `node --test 'npm/**/*.test.mjs'`, and dependency-gate Python tests.
- [x] Exercise the candidate with an isolated PTY across 429, retained cache
  age and recovery, keeping the same picker process. Verify first-use,
  installed account routing and streaming using the existing fixture scripts.
- [ ] Write the concrete changelog and research record. Main agent commits and
  pushes, checks Linux/macOS CI, merges reviewed work and publishes the next
  patch version through the existing channels.
- [ ] Download and verify published artifacts and package integrity. Install
  WSL/M3 and replace the managed proxy services while retaining native sessions
  and account pointers. Record actual running builds, not just file versions.
- [ ] Recheck usage behavior with bounded live observation, allowing provider
  restrictions to remain visible. Preserve all evidence and limitations in
  the release/PR, including any picker that could not be reloaded through its
  terminal's permitted control interface.
