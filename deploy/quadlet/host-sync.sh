#!/usr/bin/env bash
#
# Sync this directory's units, scripts and drop-ins onto the collection
# host. Run as root ON THE HOST, from a checkout:
#
#     ./deploy/quadlet/host-sync.sh
#
# Idempotent -- safe to re-run. It installs files and reloads systemd; it
# deliberately does NOT start or restart anything, because a collector or
# enricher that is mid-run should be allowed to finish. Enabling timers is
# still a separate, explicit step (see README, "Enrichers").
#
# This exists because the install steps were scattered across half a dozen
# bash blocks in the README, and a partial application is worse than none:
# #94's per-enricher timeouts do nothing without the drop-ins, and the
# OnFailure= in the templates points at a unit the host may not have.
#
# Two host-specific hazards it handles, both learned the hard way:
#
#   * Scripts here are bind-mounted into running containers, and bash
#     re-reads its script file from disk as it executes rather than
#     buffering it. Writing over a live path splices old and new content
#     together mid-run -- that cost this host ~50 minutes of a QLever index
#     build once, via `scp` straight to the destination. Every file is
#     staged beside its target and renamed in; rename swaps the directory
#     entry, so a process mid-read keeps the old inode until it closes.
#     (GNU `install` happens to unlink first and is safe the same way, but
#     that is a coreutils implementation detail, not a guarantee -- the
#     unsafe shapes are `scp`, `cp` onto the path, and `> "$dst"`.)
#   * On an SELinux host, a file moved into /etc/systemd/system keeps its
#     source context, and systemd then refuses to load it with a "Unit
#     does not exist" that is indistinguishable from a missing file.
#     restorecon runs after every rename.
set -euo pipefail

SRC="${1:-$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)}"

# PG_SYNC_ROOT exists so this can be exercised against a throwaway tree
# before it is pointed at the real host. Unset in normal use.
ROOT="${PG_SYNC_ROOT:-}"
QD="$ROOT/etc/containers/systemd"
SD="$ROOT/etc/systemd/system"
STATE="$ROOT/var/lib/packagegraph"

if [ -z "$ROOT" ] && [ "$(id -u)" -ne 0 ]; then
  echo "must run as root on the collection host" >&2
  exit 1
fi

# Under PG_SYNC_ROOT there is no systemd to talk to.
systemctl() { if [ -n "$ROOT" ]; then echo "     [dry] systemctl $*"; else command systemctl "$@"; fi; }

selinux_on() {
  command -v restorecon >/dev/null 2>&1 &&
    [ "$(getenforce 2>/dev/null || echo Disabled)" != Disabled ]
}

install_atomic() {
  local mode=$1 src=$2 dst=$3 tmp
  tmp=$(mktemp "$(dirname "$dst")/.pg-host-sync.XXXXXX")
  cat "$src" >"$tmp"
  chmod "$mode" "$tmp"
  mv -f "$tmp" "$dst"
  if selinux_on; then restorecon -F "$dst"; fi
}

echo "== retiring repology (#96)"
# Deleting the files from the repo uninstalls nothing on a host that
# already has them, and an orphaned timer keeps firing.
systemctl disable --now pg-enrich-repology.timer 2>/dev/null || true
rm -f "$SD/pg-enrich-repology.timer"
rm -f "$QD/scripts/enrichers/repology.sh"
rm -rf "$SD/pg-enrich@repology.service.d"

echo "== failure notifier (#94)"
install -d -m 755 "$QD/scripts" "$STATE/failed-units"
install_atomic 644 "$SRC/failure/pg-unit-failed@.service" "$SD/pg-unit-failed@.service"
install_atomic 755 "$SRC/scripts/unit-failed.sh" "$QD/scripts/unit-failed.sh"

echo "== unit templates"
install_atomic 644 "$SRC/enrichers/pg-enrich@.container" "$QD/pg-enrich@.container"
install_atomic 644 "$SRC/collectors/pg-collect@.container" "$QD/pg-collect@.container"

echo "== collector and enricher scripts"
install -d -m 755 "$QD/scripts/enrichers" "$QD/scripts/collectors"
for f in "$SRC"/enrichers/scripts/*.sh; do
  install_atomic 755 "$f" "$QD/scripts/enrichers/$(basename "$f")"
done
for f in "$SRC"/collectors/scripts/*.sh; do
  install_atomic 755 "$f" "$QD/scripts/collectors/$(basename "$f")"
done

echo "== per-enricher timeout drop-ins (#94)"
for d in "$SRC"/enrichers/dropins/*.service.d; do
  install -d -m 755 "$SD/$(basename "$d")"
  for f in "$d"/*.conf; do
    install_atomic 644 "$f" "$SD/$(basename "$d")/$(basename "$f")"
  done
done

echo "== timers"
for f in "$SRC"/enrichers/timers/*.timer "$SRC"/collectors/timers/*.timer; do
  install_atomic 644 "$f" "$SD/$(basename "$f")"
done

systemctl daemon-reload

echo
echo "== verification"
echo "-- timeout drop-ins installed:"
found=0
for d in "$SD"/pg-enrich@*.service.d; do
  [ -d "$d" ] || continue
  found=1
  echo "     $(basename "$d")"
done
[ "$found" -eq 1 ] || echo "     NONE -- the drop-ins did not install"
echo "-- repology remnants (expect none):"
found=0
for p in "$SD"/*repology* "$QD"/scripts/enrichers/*repology*; do
  [ -e "$p" ] || continue
  found=1
  echo "     $p"
done
[ "$found" -eq 1 ] || echo "     none"
echo "-- OnFailure= instance name resolves:"
if [ -n "$ROOT" ] || systemctl cat 'pg-unit-failed@pg-enrich@security.service' >/dev/null 2>&1; then
  echo "     ok"
else
  echo "     FAILED -- the notifier template did not install"
fi
echo "-- effective TimeoutStartSec:"
for u in security taxonomy koji; do
  printf '     %-10s %s\n' "$u" \
    "$(systemctl show "pg-enrich@$u.service" -p TimeoutStartUSec --value 2>/dev/null)"
done
echo "-- image the enricher template tracks (must be :devel-latest):"
grep -i '^Image=' "$QD/pg-enrich@.container" | sed 's/^/     /'
