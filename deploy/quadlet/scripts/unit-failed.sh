#!/bin/sh
# Record a failed unit where a human will find it, and nowhere else.
#
# Invoked by pg-unit-failed@.service, which the collector and enricher
# templates name in OnFailure=. Writes two things under
# /var/lib/packagegraph/failed-units/:
#
#   <unit>.txt   the latest failure of that unit: how it ended, when, and
#                the tail of its journal. Overwritten each time, because
#                the question being answered is "what is wrong NOW".
#   history.tsv  one line per failure, appended forever, so a unit that
#                fails every week looks different from one that failed once.
#
# Nothing leaves the host. See pg-unit-failed@.service for why (#59).
set -u

UNIT="${1:-}"
if [ -z "$UNIT" ]; then
  echo "usage: unit-failed.sh <unit>" >&2
  exit 1
fi

# Overridable only so the test harness can drive the real script; nothing
# in production sets it.
DIR="${PG_FAILED_UNITS_DIR:-/var/lib/packagegraph/failed-units}"
NOW=$(date -u +%Y-%m-%dT%H:%M:%SZ)

mkdir -p "$DIR" || exit 1

# '/' cannot appear in a unit name and '@' is awkward in a filename; the
# unit name is still recorded verbatim inside the file.
SAFE=$(printf '%s' "$UNIT" | tr '@/' '__')

# head -1 is not decoration: history.tsv is a tab-separated line per
# failure, and anything multi-line here silently corrupts every later read
# of the file.
RESULT=$(systemctl show "$UNIT" --property=Result --value 2>/dev/null | head -1)
[ -n "$RESULT" ] || RESULT=unknown

{
  echo "unit:     $UNIT"
  echo "recorded: $NOW"
  echo
  systemctl show "$UNIT" \
    --property=Result \
    --property=ExecMainStatus \
    --property=ExecMainCode \
    --property=NRestarts \
    --property=InactiveEnterTimestamp \
    --property=ActiveEnterTimestamp 2>/dev/null
  echo
  echo "--- last 100 journal lines ---"
  # Why the tail and not a pointer to journalctl: the journal is rotated,
  # and the run that explains the failure is usually the one that ages out
  # first. A timeout kill in particular leaves its evidence in the last
  # lines before SIGKILL -- see #59's koji and security transcripts.
  journalctl -u "$UNIT" -n 100 --no-pager 2>/dev/null
} > "$DIR/$SAFE.txt" 2>/dev/null

printf '%s\t%s\t%s\n' "$NOW" "$UNIT" "$RESULT" >> "$DIR/history.tsv" 2>/dev/null

# Also into the journal, tagged, so `journalctl -t pg-unit-failed` is a
# complete failure log even if the directory is lost.
logger -t pg-unit-failed "$UNIT failed (result=$RESULT); details in $DIR/$SAFE.txt" 2>/dev/null

exit 0
