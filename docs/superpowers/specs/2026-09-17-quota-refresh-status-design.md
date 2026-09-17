# Explain stalled quota readings in an open dashboard

## Observed failure

A running 0.165.6 dashboard and both managed proxies matched the installed
binary. Four displayed accounts matched a separate current quota read. A
Claude account with expired access retained an eleven-hour-old reading;
background renewal was deferred because a live native session held its login.
The dashboard erased the failed-read reason when restoring cached numbers,
and its group header counted the expired account as ready.

## Design

Keep the existing last successful reading, its observation time, and the reason
a newer reading could not be obtained. Display the reason together with the
cached age (for example, `login expired · as of 11h`). Group readiness must agree with account-row health: missing,
expired, paused, or warning states are not ready, and their old percentages
must not inflate available capacity. Preserve current spent-account/reset and
extra-usage behavior for otherwise usable credentials. Unavailable credentials
are excluded from the aggregate reset forecast as well: a quota reset does not
repair an expired, paused, or unreadable login.

When cause and age do not fit after the gauges, wrap the explanation onto
indented detail lines. Preserve the gauges and use the rendered row heights
for mouse selection, including at 144-column and 80-column terminal widths.

An expired Claude slot held by a live native session should report that renewal
is deferred, rather than explain it as an ordinary saved snapshot. Derive this
from the same read-only ownership checks used by renewal. Quota inspection must
remain read-only for credentials: no OAuth exchanges, credential writes, forced
renewal, account switching, or terminating native sessions.

## Boundaries and recovery

This change explains and correctly represents stalled reads. It does not make
an expired native credential usable. Once the owning native session supplies a
valid login, the already-open dashboard must pick up the new state and provider
reading through its existing background refresh cycle. Software self-update
and process replacement are outside this diagnosed failure.

## Validation

Reproduce the cache/summary errors before implementation. Cover expired cached
readings, other failed reads, valid/current readings, unavailable fleet members,
and an initial deferred/cached result followed by a valid native login becoming
available while the dashboard stays open. Distinguish an ordinary expired slot
from a held expired slot, asserting no OAuth or credential writes in either case.
Use synthetic credentials and local fixture providers; verify zero OAuth/model
requests from quota. Run repository formatting, Clippy, and full tests, followed
by a real PTY rendering check and an independent source review.
