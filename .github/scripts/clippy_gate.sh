#!/usr/bin/env bash
# Fork CI (12/11/2026): clippy on EVERY crate, the fork's findings told apart from upstream's.
#
# Upstream's static_analysis step (`cargo clippy --workspace --all-targets -- -D warnings`) fails
# on 5.0.3 itself and stops at the first crate with a finding, so it never reaches the crates the
# fork changed (as on 4.15). This lints the fork (HEAD) and then pristine 5.0.3 in the same
# checkout, both with --cap-lints warn (every crate is linted, nothing stops the build), with
# default features and then with all features, and hands both outputs to clippy_gate.py: a
# finding (lint or hard error) the base has at the same place with the same code, or in a hunk
# the fork rewrote with the same text, is upstream's own; any other finding fails this step, as
# does a cargo failure the base does not share. Results are annotations.
set -uo pipefail
base=5664a2dc2066f188ed2196a65261c61302635f38 # tag 5.0.3 (= upstream master, 10/11/2026)
tools=$(mktemp -d)
cp .github/scripts/clippy_gate.py "$tools/"
head=$(git rev-parse HEAD)
export CARGO_TERM_COLOR=never
rc=0
lint() { # <out> [cargo args]: clippy as upstream runs it, findings capped at warnings
  local out=$1
  shift
  cargo clippy --workspace --all-targets "$@" --message-format=json -- -D warnings --cap-lints warn \
    >"$out.jsonl" 2>"$out.err"
  local status=$?
  echo "$status" >"$out.rc"
  echo "cargo clippy $* -> exit $status ($(grep -c '"reason":"compiler-message"' "$out.jsonl") messages)"
  tail -5 "$out.err"
}
for features in default all; do
  args=()
  [ "$features" = all ] && args=(--all-features)
  git checkout -q --force "$head"
  lint "$tools/head-$features" "${args[@]}"
  git checkout -q --force "$base"
  lint "$tools/base-$features" "${args[@]}"
  git checkout -q --force "$head"
  python3 "$tools/clippy_gate.py" \
    "$tools/head-$features.jsonl" "$(cat "$tools/head-$features.rc")" \
    "$tools/base-$features.jsonl" "$(cat "$tools/base-$features.rc")" \
    "$base" "clippy $features features" || rc=1
done
exit $rc
