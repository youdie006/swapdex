# macOS peer-disconnect follow-up: 0.164.1

The final PR #31 CI passed on Linux and macOS, but the identical merged tree
failed the bodyless-GET retry regression in post-merge macOS CI:
https://github.com/youdie006/swapdex/actions/runs/34953069370

The runtime reported `io: Peer disconnected` and returned 502. That error
spelling was missing from the transient-error classifier. Two direct unit
assertions reproduced the failure locally before the repair: GET must retry
this condition and POST must not replay an accepted request.

The repair recognizes this transport error and retains the existing method
safety check. Version 0.164.0 remains an immutable prerelease candidate on
GitHub. It was not published to npm, crates.io or Homebrew and was not installed
on WSL or M3. The corrected release uses 0.164.1 and carries the full audit notes.

Validation and exact publication/install records are appended to the 0.164.1
release and its PR after the final checks. The previous candidate's failure
remains visible in its release status record.

## Candidate verification

- PASS two direct retry-classifier regressions after their recorded RED run.
- PASS `cargo test --all --locked --jobs 2`: 1,135 passed, zero failed, one
  existing ignored streaming test.
- PASS all-target locked Clippy, formatting, diff checks and Python syntax.
- PASS the enhanced installed-style routing verifier against 0.164.1: both
  providers submit an accepted POST once and recover a bodyless GET.
- PASS stock Codex provider repair, native listing/resume and HTTP recovery
  with unchanged synthetic conversation bytes.
