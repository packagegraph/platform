#!/bin/bash
# Asserts the checkpoint contract every rpm-full wrapper must satisfy.
# These are ordering and exclusion properties no Rust test can observe.
set -uo pipefail
cd "$(dirname "$0")"
fail=0

for f in *-full.sh; do
  grep -q 'pg-collect rpm-full' "$f" || continue

  # Executable lines only -- a contract satisfied by a comment is not
  # satisfied at all.
  code=$(grep -vE '^\s*#' "$f")

  # Exactly three mirrors: startup warm, periodic loop, final sync.
  n_mirror=$(printf '%s\n' "$code" | grep -c 'mc mirror')
  [ "$n_mirror" -eq 3 ] || { echo "FAIL $f: expected 3 mc mirror calls, found $n_mirror"; fail=1; }

  # Every one of them must exclude the checkpoint subtree, both directions.
  n_excl=$(printf '%s\n' "$code" | grep 'mc mirror' | grep -c -- "--exclude 'output/\*'")
  [ "$n_excl" -eq "$n_mirror" ] || { echo "FAIL $f: $((n_mirror-n_excl)) mirror(s) missing output/* exclusion"; fail=1; }

  # Direction: startup pulls remote->local; the other two push local->remote.
  printf '%s\n' "$code" | grep 'mc mirror' | head -1 | grep -q '"${MINIO_CACHE}/" "${CACHE_DIR}/"' \
    || { echo "FAIL $f: first mirror is not remote->local (startup warm)"; fail=1; }
  n_push=$(printf '%s\n' "$code" | grep 'mc mirror' | grep -c '"${CACHE_DIR}/" "${MINIO_CACHE}/"')
  [ "$n_push" -eq 2 ] || { echo "FAIL $f: expected 2 local->remote mirrors, found $n_push"; fail=1; }

  # Commit appears exactly once, is executable, and follows the upload.
  n_ck=$(printf '%s\n' "$code" | grep -c 'checkpoint commit')
  [ "$n_ck" -eq 1 ] || { echo "FAIL $f: expected exactly 1 'checkpoint commit', found $n_ck"; fail=1; }

  up=$(grep -nE '^[^#]*upload-nt\.sh' "$f" | head -1 | cut -d: -f1)
  ck=$(grep -nE '^[^#]*checkpoint commit' "$f" | head -1 | cut -d: -f1)
  if [ -z "$up" ]; then
    echo "FAIL $f: no executable upload-nt.sh line"; fail=1
  elif [ -z "$ck" ]; then
    echo "FAIL $f: no executable 'checkpoint commit' line"; fail=1
  elif [ "$ck" -lt "$up" ]; then
    echo "FAIL $f: commit (line $ck) precedes upload (line $up)"; fail=1
  fi

  # The upload must be able to fail the script. '|| true' would let a failed
  # publication reach the commit and retire a generation that never shipped.
  printf '%s\n' "$code" | grep 'upload-nt.sh' | grep -qE '\|\|\s*true' \
    && { echo "FAIL $f: upload-nt.sh is guarded by '|| true'"; fail=1; }

  # set -e is what makes the ordering load-bearing.
  grep -qE '^set -[a-z]*e' "$f" || { echo "FAIL $f: no 'set -e'"; fail=1; }
done

[ "$fail" -eq 0 ] && echo "All rpm-full wrappers satisfy the checkpoint contract."
exit "$fail"
