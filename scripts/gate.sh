#!/bin/sh
# The gate, the way CI runs it, with the failing test NAMED.
#
# This existed as a shell one-liner that grepped `^test result` and nothing
# else. When one of sixty-one proxy tests failed once under load, the line it
# kept said "1 failed" and the name was gone - and the failure did not
# reproduce, so there was nothing left to chase. A gate that cannot say what
# broke is a gate that gets argued with instead of read.
#
#   sh scripts/gate.sh [outfile]
#
# Exits non-zero if any step fails. Runs each test binary separately with
# --jobs 1: a full `cargo test` builds them all at once and has been killed by
# the memory watchdog on this machine.
set -u
out="${1:-/dev/stdout}"
fail=0

say() { printf '%s\n' "$*" >>"$out"; }

: >"$out"
say "== fmt"
if cargo fmt --check >/dev/null 2>&1; then say "  ok"; else
  say "  PROBLEM - run \`cargo fmt\`"
  cargo fmt --check 2>&1 | head -20 >>"$out"
  fail=1
fi

say "== clippy"
if cargo clippy --all-targets -- -D warnings >/dev/null 2>&1; then say "  ok"; else
  say "  PROBLEM"
  cargo clippy --all-targets -- -D warnings 2>&1 | grep -E '^(error|warning)' | head -20 >>"$out"
  fail=1
fi

run_tests() {
  label="$1"
  shift
  log=$(mktemp)
  "$@" >"$log" 2>&1
  code=$?
  say "== $label"
  grep -E '^test result' "$log" | sed 's/^/  /' >>"$out"
  if [ "$code" -ne 0 ]; then
    fail=1
    # The whole point: which test, and what it said.
    say "  FAILED:"
    sed -n '/^failures:$/,/^test result/p' "$log" | grep -E '^\s+[a-z_0-9:]+$' | sed 's/^/    /' >>"$out"
    grep -E 'panicked at|assertion .* failed' "$log" | head -6 | sed 's/^/    /' >>"$out"
  fi
  rm -f "$log"
}

run_tests lib cargo test --lib --jobs 1
for t in agreement release_metadata switch proxy run refresh no_leak pipe soak; do
  run_tests "$t" cargo test --test "$t" --jobs 1
done

say ""
if [ "$fail" -eq 0 ]; then say "GATE: green"; else say "GATE: RED"; fi
exit "$fail"
