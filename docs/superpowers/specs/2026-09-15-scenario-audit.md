# Account and upgrade scenario audit

The user approved fixing the two reproduced 1.0 blockers and asked for more
debugging across different situations. This continues the existing permission
to fix, publish, install and verify Swapdex. It does not declare 1.0 ready.

## Required behavior

- A percentage suffix is literal: `0.5%` means a fraction of `0.005`, `1%`
  means `0.01`, while unsuffixed `0.5` remains a fraction of `0.5`. Displayed
  settings retain meaningful fractional percentages. Invalid input cannot
  alter the prior setting.
- The runtime must honor valid thresholds below 5% for both stored settings
  and explicit proxy flags. Invalid stored fractions are ignored; invalid
  explicit flags fail before starting a listener. This closes the additional
  silent 5% floor found while tracing the original percentage report.
- Disabling paid extra usage must not exhaust an otherwise allowed Claude
  account. Explicit rejection of the included plan or its windows still
  excludes an exhausted account. Cover actual routing as well as parsing.
- A service installed through Homebrew must retain an executable location
  after an upgrade removes the old Cellar version. Verify a candidate stable
  link identifies the running executable before using it; do not trust an
  unrelated executable found on PATH. Other installation methods keep working.
- Quota observations, token renewal, native login ownership and serving
  selection must remain consistent under concurrent and failed operations.
  Audit these scenarios first and change behavior only for demonstrated bugs.
- A successful quota-triggered renewal must supply its new credential to the
  same usage read. Same-name saved profiles must not suppress eligible renewal
  of their authoritative slots. Failed/deferred renewal preserves ownership.
- A settings write that cannot obtain its lock must leave the file untouched
  and report a lock failure, rather than claiming success after an unsafe write.
- Cache writers must preserve independent observations across simultaneous
  usage reads and served-response updates. Atomic replacement alone does not
  serialize read/modify/write operations.
- An automatic move blocked by pause or headroom rules must not be presented
  as a provider refusal without evidence. Keep the existing Roomiest movement
  margin; use ConsumeFirst to isolate subpercent threshold behavior in tests.

## Verification boundaries

Regression fixtures use disposable roots and loopback providers. They must
not touch native credentials, account choices, normal service ports or user
conversations. Preserve diagnostics about unsupported and deferred conditions.
Verify both Linux and macOS via CI and both installed targets after release.
Record real provider checks separately from simulated login and renewal.

## Delivery

Keep file ownership separate during parallel work, review integrated changes,
and retain the reproduction commands, failed attempts, limitations and final
results in a repository audit and the version-specific release/PR record.
Ship a patch release after checks pass, without a 1.0 stability claim.
