#!/bin/sh
# Ban/unban helper for fail2ban's custom nftables-shared-set action.
# Targets the SAME banned_ips/banned_ips6 sets that CrowdSec's bouncer
# also writes to (see firewall/nftables.conf) -- one shared enforcement
# point regardless of which tool decided to ban an address, so
# `nft list set inet filter banned_ips` shows every ban either tool made.
#
# Runs inside the fail2ban container (needs --network host + NET_ADMIN,
# see fail2ban.container) so `nft` here acts on the real host ruleset.
set -eu

action="$1"
ip="$2"
bantime="${3:-3600}"

case "$ip" in
  *:*) set_name="banned_ips6" ;;
  *)   set_name="banned_ips" ;;
esac

case "$action" in
  ban)
    nft add element inet filter "$set_name" "{ $ip timeout ${bantime}s }"
    ;;
  unban)
    # Already-expired elements make this a harmless no-op, not an error.
    nft delete element inet filter "$set_name" "{ $ip }" 2>/dev/null || true
    ;;
  *)
    echo "usage: $0 {ban|unban} <ip> [bantime_seconds]" >&2
    exit 1
    ;;
esac
