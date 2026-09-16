# Keep disconnected clients from terminating the proxy

## Observed failure

After installing 0.165.3 on WSL, the Codex service repeatedly restarted while
the Claude service remained up. An append-only trace of process signals and
exits captured the Codex process being killed by SIGPIPE. The trace collected
no request bodies or credentials. All temporary service overrides were removed.

`main` restores the default SIGPIPE disposition for ordinary Unix output
pipelines. That process-wide setting also reaches the proxy. A broken socket
write can consequently terminate every connection served by the process.
Library tests inherit Rust's ignored SIGPIPE disposition and miss the executable
startup policy. Supervisor restarts and older caller builds were considered;
the observed termination was SIGPIPE, not a takeover SIGTERM.

## Required behavior

- A broken client connection must not terminate the shared proxy. Both Claude
  and Codex must keep serving subsequent requests in the same process.
- Establish the proxy's signal policy before starting any listener or worker.
- Preserve normal CLI behavior when stdout is piped to a short reader, and
  preserve SIGINT/SIGTERM cleanup and termination.
- Keep account selection, credential handling, retry rules and stream framing
  unchanged. Do not add a supervisor workaround or dependency.

## Design decision

Restore SIGPIPE ignoring at proxy entry. This makes broken socket writes surface
as ordinary I/O errors. Removing the CLI policy globally would regress Unix
pipelines; patching individual writes would leave other proxy I/O paths exposed.
The fix is confined to the proxy process and applies to both tools.

## Verification and delivery

Use real executable subprocesses with isolated stores and fake loopback
providers. Capture a failing SIGPIPE regression on the released behavior, then
verify the same process handles a fresh request after the fix. Exercise actual
client disconnection as well as the signal policy, retain the CLI pipe test,
and run the required Rust and installed-binary checks. Never signal a user's
native assistant session or use real accounts in regression fixtures.

Release as 0.165.4 after review and CI. Publish all existing channels and install
the exact release on WSL and M3. Record runtime identity, restart observations,
installed checks and small authorized live requests in the release/PR ledger.
The immutable 0.165.3 release remains a separate delivery record with its
post-install SIGPIPE finding disclosed.
