#!/usr/bin/env bash
# Fork CI helper (27/09/2026): run a command; when it fails, publish a compact summary as
# annotations, which GitHub's public check-runs API serves without a login (job logs need
# admin rights on the repository): each test binary with its failing tests, then the
# distinct error lines.
set -o pipefail
export CARGO_TERM_COLOR=never
log=$(mktemp)
"$@" 2>&1 | tee "$log"
rc=${PIPESTATUS[0]}
if [ "$rc" -eq 124 ] || [ "$rc" -eq 137 ]; then  # timeout(1) INT, then KILL: where it hung
  sed 's/\x1b\[[0-9;]*m//g' "$log" | tail -60 | sed -e 's/%/%25/g' -e 's/\r//g' > "$log.tail"
  printf '::error title=TIMEOUT %s::%s\n' "$*" "$(awk '{printf "%s%%0A", $0}' "$log.tail")"
fi
if [ "$rc" -ne 0 ]; then
  sed 's/\x1b\[[0-9;]*m//g' "$log" > "$log.plain"
  {
    grep -E '^\s+Running |^test result: FAILED|^failures:$|^    [A-Za-z0-9_:]+$' "$log.plain" \
      | grep -B1 -A12 -E 'test result: FAILED|^failures:$' | grep -v '^--$'
    grep -E -A6 '^(error|warning)(\[[A-Za-z0-9]+\])?:|could not compile' "$log.plain"
  } | awk '!seen[$0]++' | head -240 | sed -e 's/%/%25/g' -e 's/\r//g' > "$log.err"
  split -l 40 "$log.err" "$log.part."
  for p in "$log".part.*; do
    [ -f "$p" ] || continue
    printf '::error title=%s::%s\n' "$*" "$(awk '{printf "%s%%0A", $0}' "$p")"
  done
fi
exit "$rc"
