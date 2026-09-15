# Managed authentication headers: 0.165.2

## Requirement and reproduction

After installing 0.165.1, a real isolated native Claude probe used a placeholder
API key against the managed WSL proxy. The proxy added the selected OAuth token
but retained `x-api-key`; Anthropic rejected the placeholder and the managed
account was temporarily sidelined for a failure caused by another credential.
The probe returned 401 and zero model tokens. Only the Claude proxy was restarted
to clear that in-memory state; all account selection pointers remained unchanged.
Ordinary OAuth Claude requests before this extra probe had returned 200.

This is separate from the user's earlier waiting indicator: the pre-install
WSL log recorded one response-header timeout (the existing budget is 300 seconds)
and two 400 responses for 1,000,120 input tokens exceeding the 1,000,000-token
window. A shared proxy log does not identify which project produced that request.
No native conversation was altered or automatically compacted. The installed
70% auto-compaction setting was observed; that is not proof it compacted a
particular request. The user was directed to native `/compact` and asked which
project was waiting.

Managed requests must discard every case-insensitive `x-api-key` header before
adding their selected credential. Explicit `serve --off` and authentication
exchanges must retain client headers. The correction must apply to both Claude
and Codex without changing account-selection or retry rules.

## Implementation and delivery plan

- [x] Reproduce with a real loopback upstream that rejects foreign API keys.
  Both managed tool cases failed with 401. Four passthrough/auth-exchange
  controls passed. Each case sends two header spellings and duplicate fields.
- [x] Filter the header only after both client-authentication branches.
- [x] Run the six cases, all Rust tests, Clippy, formatting, Python/npm checks,
  dependency audit and candidate first-use/routing/stock Codex resume checks.
- [ ] Publish an immutable follow-up patch; retain the completed 0.165.1 record.
  Require Linux/macOS CI before merging and all four release builds/checksums.
- [ ] Verify all package channels, install exact versions on WSL/M3, preserve
  account choices, validate actual service executables and repeat installed
  routing/resume checks plus the formerly failing native Claude probe.

Version-specific release and PR records carry actual source commits, channels,
installed versions, process/hash checks and failed/unavailable verification.
Do not equate a small successful request with proving latency of a million-token
conversation, independent billing attribution or fresh provider login UI.

## Candidate results

PASS `cargo test --all --locked --jobs 2`: 1,198 passed, zero failed and two
ignored entries (one pre-existing streaming test and one service helper run
explicitly by its parent). PASS locked all-target Clippy with warnings denied,
formatting/diff checks and generated man-page comparison. PASS 22 Python tests,
seven npm tests, locked dependency policy/bundled roots and `cargo audit`.

PASS six isolated first-use autostart journeys, installed-routing simulation
(A-B-A for each tool, accepted POST not replayed, safe GET retry), and stock
Codex listing/resume with preserved conversation bytes. Real provider tests
remain separate and are recorded after exact-version deployment.
