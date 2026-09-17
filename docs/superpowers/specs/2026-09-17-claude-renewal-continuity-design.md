# Claude renewal continuity

## Authorized requirement

Keep registered Claude logins renewed without requiring the user to close
existing conversations or repeat browser login. The previous restoration and
quota display release did not complete this requirement.

## Decision

Use Claude's existing refresh exclusion protocol at the actual credential
store. Establish one durable authority when a slot and a live native home are
proven to hold the same identity and refresh generation. Future readers and
renewers must follow that authority even after the native process exits.

Alternatives considered:

- Continuing to defer whenever a process exists leaves long-idle sessions
  without a guaranteed renewal owner and does not meet the requirement.
- Periodically copying complete credentials leaves multiple potential
  refreshers and can resurrect a retired generation.
- Cooperative locking plus one credential authority preserves native OAuth
  behavior and provides a testable read/exchange/write boundary.

## Native synchronization

Installed Claude 2.1.271 through 2.1.274 acquire two directory locks in order:
`<storage>/.oauth_refresh.lock`, then `<realpath(storage)>.lock`. Their lock
heartbeats run every five seconds and the stale interval is sixty seconds.
After acquisition they invalidate caches and reread credentials before deciding
whether to exchange a token. Swapdex must take those same locks, retain them
through persistence, and recheck the selected identity and credential after
acquisition. Lock contention or lost ownership must never authorize an
uncoordinated exchange.

## Authority

Authority selection requires both provider account and organization identity,
plus agreement on the current refresh generation. Independently issued logins
must remain independent. The descriptor distinguishes a default native home
from an explicit config directory, including the identity path and Keychain
service. Native storage and session configuration are separate concerns;
launch routing must preserve native session history and configuration.

Descriptor writers serialize on a private per-slot metadata lock and retain
the copied slot's native refresh locks to coordinate with renewal of that
unbound slot. They do
not acquire the chosen source's refresh locks: selecting that source performs
no credential writes or OAuth exchange and remains possible while its native
client is renewing. Repeated identity and exact generation reads guard the
initial association; subsequent generations remain with that source. OAuth
exchange and credential persistence still require both native refresh locks
of the effective credential store.

Once authority is established, an obsolete slot copy cannot become eligible
for renewal merely because its former native process exited. Missing or
mismatched authority remains an explicit error. New login and logout behavior
must be covered before enabling automatic migration in a real installation.

## Validation

Use isolated roots and synthetic credentials. Verify exclusion with stock
native binaries, two consecutive renewals, live-session continuation, process
exit and relaunch, interrupted refresh, changed identities and generations,
independent chains, and Keychain source selection. No real provider inference
is needed for these tests. Keep all existing account-routing and credential
replacement checks passing. Source verification precedes publication and
installation; those are separate outcomes.
