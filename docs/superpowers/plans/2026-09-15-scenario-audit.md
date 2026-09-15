# Account and upgrade scenario audit implementation plan

> **For agentic workers:** REQUIRED: Use superpowers:subagent-driven-development
> for implementation and independent review. Only the main agent commits or
> pushes. Steps use checkboxes for tracking.

**Goal:** Fix reproduced account-routing and percentage bugs, validate upgrade
and renewal boundaries, and install the verified patch on WSL and M3.

**Architecture:** Retain the current synchronous proxy, settings and service
modules. Add focused regression tests before fixes. Independently audit token
renewal and apply a bounded correction only when an observable failure is found.

**Tech Stack:** Rust, cargo, POSIX shell, Python loopback fixtures, systemd,
launchd, GitHub Actions, npm, crates.io and the existing Homebrew tap.

## Chunk 1: Reproduce and fix

- [x] Main: baseline `cargo test --test run --locked --jobs 2
  threshold_setting_accepts_both_notations_and_off` passes on 0.165.0.
- [x] Main owns `src/commands.rs`, `src/settings.rs`, `tests/run.rs`,
  `tests/proxy.rs`, the threshold startup copy in `src/proxy/mod.rs` and
  release/audit documents.
  Add failing threshold cases for fractional percentages, 1%, mixed notation,
  malformed suffixes, non-finite/out-of-range values and unchanged settings on
  error. Run `cargo test --test run --locked --jobs 2 threshold_` before and
  after the minimal parser/display correction.
  Also run `cargo test --test proxy --locked --jobs 2 threshold` to prove
  stored and pinned 0.5% settings actually rotate an account at 1% usage and
  reject invalid explicit thresholds before any listener starts.
- [x] Overage worker owns `src/proxy/ratelimit.rs` and a dedicated regression
  test file. Reproduce allowed plan + rejected overage, then preserve available
  accounts while retaining explicit plan-window rejection. Test routing with
  loopback requests, including an exhausted account followed by an available
  extra-usage-disabled account. Run its focused tests and all-target Clippy.
- [x] Service worker owns `src/service.rs` and a dedicated regression test
  file. Reproduce a versioned Cellar executable removed after service setup.
  Select only a verified stable Homebrew path, test malicious/unrelated links,
  spaces, existing non-Homebrew installs and replacement versions. Run focused
  service tests and all-target Clippy.
- [x] Renewal explorer is read-only: trace concurrent quota/proxy/manual
  renewals, native-holder deferral, replaced credentials, cancelled requests,
  provider failures and serving-generation handling. Distinguish existing
  protections, observed bugs and untested real-provider behavior. Report exact
  commands and file citations; no credentials or private harness state.
- [x] Review the spec/plan and each implementation for requirements and code
  quality. Add separately reproduced fixes to this plan if the audit finds more.

### Additional scenarios confirmed during the audit

- [x] Main: threshold/parser/runtime and neutral movement diagnostics have nine
  passing focused tests, after six original threshold failures and a separately
  reproduced false-refusal message. The movement fixture explicitly selects
  ConsumeFirst; the Roomiest control preserves its existing 10-point margin.
- [x] Main owns `src/settings.rs`, `src/store.rs`, `src/main.rs` and
  `tests/settings_contention.rs` for long contention and an unwritable lock.
  Both regressions returned success and changed settings before repair.
  Require exit 4 without changing the file; retry after unlocking must succeed.
- [x] Quota worker owns only `commands::quota`, the module documentation in
  `src/quota.rs`, and `tests/quota_renewal.rs`. Verify slot-only and same-name
  profile+slot renewal, first-read fresh-token use, failed/deferred renewal and
  a replacement credential during the exchange.
- [x] Cache worker owns `src/quota_cache.rs` and dedicated concurrency tests.
  Reproduce distinct-account and reset/measurement merge loss, then serialize
  all cache mutation entry points without taking the whole-store lock.
  Prove thread and process writers preserve each other's updates and tool
  namespaces remain isolated; retain the best-effort cache failure boundary.
- [x] Record successful refresh coordination, rejection-generation and late
  request selection checks from the read-only audit. The failed-read pacing
  timestamp is an attempt interval; existing measurements retain their ages.
  Do not call the intentional throttle backoff a defect without a reproduction.

- [x] Independent review reproduced two additional diagnostic paths: a disabled
  alternative was labeled refused after another account's 403, and unmeasured
  alternatives disappeared from an all-above-threshold claim. Both loopback
  tests failed before repair and passed after preserving the missing evidence.

## Chunk 2: Integrate and verify

- [x] Record every behavior correction under the patch release in CHANGELOG.
- [x] Run `cargo test --all --locked --jobs 2`, locked all-target Clippy with
  warnings denied, fmt/diff checks, Python and npm automation, dependency policy
  and cargo audit. Record all unavailable or failed checks honestly.
- [x] Run first-use autostart in an isolated network namespace and installed
  routing/resume fixtures using the candidate native executable.
- [ ] Save an audit record with the scenario matrix, red/green evidence and
  remaining limits. Commit and push; review and require Linux/macOS CI before
  merging the verified source.

## Chunk 3: Publish and install

- [ ] Publish a patch tag and version-specific notes; validate four release
  archives, crate, all npm packages and the Homebrew formula independently.
- [ ] Install the exact patch on WSL/M3 while retaining account selections;
  verify native hashes, service executable paths, versions and listeners.
- [ ] Verify shell installation, first-use, routing and stock Codex resume on
  both installed targets. Exercise the Homebrew upgrade path in isolation.
- [ ] Record real model/renewal checks separately from simulated flows and
  preserve final source, publication and running-service evidence in release
  and PR records. Do not claim fresh OAuth UI verification without performing it.
