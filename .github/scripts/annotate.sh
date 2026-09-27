#!/usr/bin/env bash
# Fork CI helper (27/09/2026): run a command; when it fails, publish its error lines as
# annotations, which GitHub's public check-runs API serves without a login (job logs need
# admin rights on the repository).
set -o pipefail
export CARGO_TERM_COLOR=never
log=$(mktemp)
"$@" 2>&1 | tee "$log"
rc=${PIPESTATUS[0]}
if [ "$rc" -ne 0 ]; then
  sed 's/\x1b\[[0-9;]*m//g' "$log" \
    | grep -E -A8 '^(error|warning)(\[[A-Za-z0-9]+\])?:|panicked at|^---- .* stdout ----|^failures:|test result: FAILED' \
    | head -240 | sed -e 's/%/%25/g' -e 's/\r//g' > "$log.err"
  split -l 40 "$log.err" "$log.part."
  for p in "$log".part.*; do
    [ -f "$p" ] || continue
    printf '::error title=%s::%s\n' "$*" "$(awk '{printf "%s%%0A", $0}' "$p")"
  done
fi
exit "$rc"
