# Refresh usage panels while they remain open

## Problem and scope

The Usage (`u`) and Quota (`%`) TUI panels run a synchronous child command once
when opened. Leaving a panel open leaves its numbers unchanged indefinitely,
and the initial read blocks navigation. The main account list already polls
local state and reads quota asynchronously.

This change implements the user's existing request to fix stale WSL usage
displays. It does not update a running executable in place or change account
selection. Attribution of direct Codex jobs is a separate investigation.

## Behavior

- Read Usage and Quota off the event loop on entry and every 45 seconds after
  the previous read completes, using the existing quota cadence.
- Keep the last completed content visible during a refresh; preserve scroll
  position and clamp it if the result becomes shorter.
- Allow `r` to request a refresh without leaving the panel. Coalesce requests
  while a read is in progress. Navigation and quit remain responsive.
- Discard results from an abandoned panel. Recover from a disconnected worker
  channel on a bounded cadence, without an infinite loading state or hot loop.
- Keep Doctor as an explicit diagnostic snapshot.
- Make freshness visible with a concise refresh status; do not label cached
  content as a newly successful read.

## Implementation and alternatives

Use a small, testable panel refresh state with an injectable clock. Production
owns the child process and polls its completion without blocking input; test
contexts can deliver controlled results through standard-library channels.
Each child has its own process group and anonymous temporary output files.
Dropping a panel stops and reaps its read, including helpers, without pipe
buffer deadlocks. Reuse existing render/layout code and dependencies. Blocking
periodic reads would freeze input; manual refresh alone would not meet the
requested automatic behavior. Preserve the restart guidance when installation
replaces a running picker's executable.

## Verification

Regression tests must cover idle refresh, a delayed result while navigation is
available, repeated manual refresh, exit/reentry with an old result, scroll
preservation/clamping, and a disconnected channel. Use fake homes and controlled
readers; no model requests or real account changes. Run the repository Rust
test, clippy and formatting checks. Verify the installed WSL executable with a
PTY or equivalent input/render harness before recording installation as done.
