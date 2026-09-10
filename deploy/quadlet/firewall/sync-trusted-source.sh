#!/bin/bash
# Resolves TRUSTED_HOSTNAMES (space-separated, from trusted-ips.env) and
# keeps TWO independent things in sync with the result:
#
# 1. The inet filter trusted_ips nftables set -- bypasses the
#    connection-rate pre-filter only. Does NOT prevent a ban: nftables.conf's
#    input chain checks banned_ips before trusted_ips, and this has no
#    visibility into nginx's own application-layer rate limiting either.
# 2. fail2ban's LIVE ignoreip list on every jail (via `fail2ban-client set
#    <jail> addignoreip/delignoreip`, not just jail.local's static
#    baseline) -- this is what actually prevents a ban from being issued
#    for this source in the first place. See the comment above ignoreip
#    in fail2ban/jail.local for why both exist and neither substitutes for
#    the other.
#
# Both re-resolve on a timer (see sync-trusted-source.timer) rather than
# being a one-time static rule, since a dynamic-DNS-tracked source's IP
# changes over time and a hardcoded entry would silently go stale.
#
# Flush-and-rebuild for the nftables set (cheap, can't accumulate stale
# entries). fail2ban's ignoreip has no such bulk-replace operation, so
# this diffs against the previous run's resolved set (state file) and
# issues targeted add/del calls only for what changed.
set -euo pipefail

ENV_FILE="${1:-/etc/containers/systemd/scripts/trusted-ips.env}"
# NOT under /var/lib/fail2ban/ -- that directory is labeled
# fail2ban_var_lib_t for fail2ban's own confined SELinux domain, and this
# script (an unrelated systemd oneshot) gets a real permission denial
# writing there even as root. Confirmed live.
STATE_FILE="${2:-/var/lib/packagegraph/.trusted-source-last-sync}"
JAILS="sshd sparql-proxy-abuse"

# shellcheck source=/dev/null
source "$ENV_FILE"

if [ -z "${TRUSTED_HOSTNAMES:-}" ]; then
  echo "TRUSTED_HOSTNAMES not set in $ENV_FILE -- nothing to do"
  exit 0
fi

ips=()
for host in $TRUSTED_HOSTNAMES; do
  resolved=$(getent ahostsv4 "$host" | awk '{print $1}' | sort -u)
  if [ -z "$resolved" ]; then
    echo "WARNING: $host did not resolve, leaving it out of this sync" >&2
    continue
  fi
  while read -r ip; do
    ips+=("$ip")
  done <<< "$resolved"
done

# --- 1. nftables trusted_ips (flush and rebuild) ---
nft flush set inet filter trusted_ips
if [ "${#ips[@]}" -gt 0 ]; then
  elements=$(IFS=,; echo "${ips[*]}")
  nft add element inet filter trusted_ips "{ $elements }"
  echo "trusted_ips synced: $elements"
else
  echo "No hostnames resolved -- trusted_ips is now empty"
fi

# --- 2. fail2ban live ignoreip (diff against last run) ---
if ! fail2ban-client ping >/dev/null 2>&1; then
  echo "fail2ban not running -- skipping ignoreip sync (nftables trusted_ips is still current)"
  exit 0
fi

old_ips=()
if [ -f "$STATE_FILE" ]; then
  while read -r ip; do
    [ -n "$ip" ] && old_ips+=("$ip")
  done < "$STATE_FILE"
fi

for old_ip in "${old_ips[@]+"${old_ips[@]}"}"; do
  if ! printf '%s\n' "${ips[@]+"${ips[@]}"}" | grep -qx "$old_ip"; then
    for jail in $JAILS; do
      fail2ban-client set "$jail" delignoreip "$old_ip" >/dev/null 2>&1 || true
    done
    echo "ignoreip removed (stale): $old_ip"
  fi
done

for ip in "${ips[@]+"${ips[@]}"}"; do
  if ! printf '%s\n' "${old_ips[@]+"${old_ips[@]}"}" | grep -qx "$ip"; then
    for jail in $JAILS; do
      fail2ban-client set "$jail" addignoreip "$ip" >/dev/null 2>&1 || true
    done
    echo "ignoreip added: $ip"
  fi
done

mkdir -p "$(dirname "$STATE_FILE")"
printf '%s\n' "${ips[@]+"${ips[@]}"}" > "$STATE_FILE"
