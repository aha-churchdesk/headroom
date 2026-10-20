#!/bin/bash
# Run every suite: the same command locally and in CI.
#
#   ./scripts/test.sh             fmt, clippy, all tests (Rust and the node UI tests)
#   ./scripts/test.sh --coverage  the same, measured; fails if core drops below the gate
#
# Core holds every decision (classification, admission, guard, scheduler,
# hook dialects), so it carries the gate. The daemon's sampler and the CLI's
# process handling are exercised end to end, but parts of them (the FFI
# error paths, the terminal hand-off) can't be reached on a CI runner, so
# their numbers are reported, not gated.
set -uo pipefail
cd "$(dirname "$0")/.."

CORE_LINE_GATE=90

command -v mise >/dev/null 2>&1 && RUN="mise exec --" || RUN=""
status=0
step() { echo; echo "── $1 ─────────────────────────────────────────"; }

step fmt
$RUN cargo fmt --all --check || status=1

step clippy
$RUN cargo clippy --workspace --all-targets --quiet -- -D warnings || status=1

if [ "${1:-}" != "--coverage" ]; then
  step tests
  $RUN cargo test --workspace --quiet 2>&1 | grep -Ev '^\s*$|^running 0 tests|0 passed; 0 failed' || true
  # grep swallows cargo's exit status; ask again, quietly, for the verdict.
  $RUN cargo test --workspace --quiet >/dev/null 2>&1 || { echo "FAILED"; status=1; }
else
  step "tests + coverage"
  $RUN cargo llvm-cov clean --workspace >/dev/null 2>&1
  # main.rs files are argument parsing and the event loop; their decisions
  # live in tested modules.
  if ! $RUN cargo llvm-cov --workspace --quiet \
      --ignore-filename-regex '(main\.rs|headroom-hog\.rs)$' \
      --summary-only 2>&1 | tee target/coverage-summary.txt | grep -E '^(Filename|TOTAL|core/|headroom/|headroomd/)'; then
    status=1
  fi
  $RUN cargo llvm-cov report --html --ignore-filename-regex '(main\.rs|headroom-hog\.rs)$' >/dev/null 2>&1 \
    && echo "html report: target/llvm-cov/html/index.html"

  # Line coverage of core alone, from the summary's per-file rows:
  # columns are … Lines, Missed Lines, Cover … (cover is the 10th field).
  core=$(awk '$1 ~ /^core\// { lines += $8; missed += $9 } END { if (lines) printf "%.1f", 100 * (lines - missed) / lines }' target/coverage-summary.txt)
  echo
  echo "core line coverage: ${core:-?}% (gate ${CORE_LINE_GATE}%)"
  if [ -z "$core" ] || awk -v c="$core" -v g="$CORE_LINE_GATE" 'BEGIN { exit !(c < g) }'; then
    echo "core coverage below the gate"
    status=1
  fi
fi

# The popover, rendered from states captured in real runs.
step ui
node tests/ui/ui.test.js || status=1

echo
if [ $status -eq 0 ]; then echo "all suites passed"; else echo "FAILURES"; fi
exit $status
