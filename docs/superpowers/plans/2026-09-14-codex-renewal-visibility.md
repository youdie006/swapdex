# Codex deferred renewal visibility implementation plan

> For agentic workers: use superpowers:subagent-driven-development. Implement in this isolated branch; only the main agent may commit or push.

**Goal:** Warn before access expiry when the existing Codex keep-alive cannot verify renewal because a local session holds the account.

**Architecture:** Preserve the existing 48-hour window, 30-minute timer and account-aware refresh guard. Surface a deferred result separately from success and definitive rejection. Derive the list/picker notice from current local evidence so it clears when the holding process exits or a replacement login leaves the renewal window. Never infer remote rotation from `iat`/`last_refresh`, force a refresh as a health probe, or label a deferred account revoked.

**Tech Stack:** Rust, fake OAuth curl, temporary stores, synthetic process and PTY fixtures.

## Confirmed behavior

- [x] Installed 0.158.0 detects a fake OAuth rejection with 47 hours of access validity remaining.
- [x] With a synthetic local Codex process holding that same due slot, keep-alive exits 0 with `every account has time left - nothing needed renewing`, and `ls` has no warning.
- [x] Review established that remote rotation can leave every local byte unchanged; local timestamps alone cannot prove it occurred.

## Implementation

- [x] Add a focused regression for due-but-in-use Codex renewal; assert the exchange is never sent and auth bytes stay identical.
- [x] Keep deferred renewals separate from failures in keep-alive results, human output and proxy logs. Do not change the timer or remove the guard.
- [x] Expose a current, provider-scoped deferred-renewal notice through shared account health for human/JSON `ls` and the picker, with rejection and expiry retaining stronger precedence.
- [x] Verify the notice clears once the process exits or the credential has ample time left, including a same-account holder in another directory.
- [x] Retain silent behavior for healthy credentials outside the window, and distinguish unrelated providers/accounts.
- [x] Reproduce the duplicate-slot guard claim issue; preflight held due slots so every copy reports deferral without an OAuth request.
- [x] Clarify documentation: early renewal is available, skipped verification is visible, and instant remote revocation detection needs an external signal.
- [x] Stabilize the existing provider deletion regression by waiting for rendered readiness and confirmation prompts before sending keys; retain the product's paste guard.
- [x] Record the user's requirement for durable change, validation and deployment history in shared agent instructions and the contribution guide.

## Acceptance and review

- [x] Run focused RED then GREEN tests and inspect rendered picker output with synthetic accounts.
- [x] Run `cargo fmt --all -- --check`, `cargo clippy --all-targets -- -D warnings`, `cargo test --all`, Python workflow tests and npm tests.
- [x] Independently review the integrated diff; keep real accounts and services unchanged during development.
- [x] Prepare the validated repair branch for commit/push. Track integration, release and installation results in the PR and version-specific release record, following the user's continuing update request.

All manual development runs use `SWAPDEX_ROOT` and fake credentials/network endpoints. No real OAuth refresh, probe, login or credential copying is part of verification.

Validation completed: 994 Rust tests passed (1 ignored), 22 Python workflow tests passed, 7 npm tests passed, and four independent synthetic CLI/list/PTY acceptance checks passed. The same-account duplicate-slot and provider-picker timing findings were reproduced before their repairs and passed afterward.
