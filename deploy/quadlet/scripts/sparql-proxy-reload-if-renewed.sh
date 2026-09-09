#!/bin/bash
# Runs on the HOST (systemd ExecStartPost for
# sparql-proxy-certbot-renew.service), NOT inside a container. certbot's
# --deploy-hook runs inside the certbot container and has no way to reach
# the nginx container directly, so it just drops a marker file in the
# shared certs volume; this script checks for that marker and reloads
# nginx (re-reads the renewed cert without dropping connections) only when
# it's actually present -- most runs are a no-op (cert not due for renewal
# yet) and must not bounce nginx for no reason.
set -euo pipefail

MARKER="/var/lib/packagegraph/sparql-proxy-certs/.renewed-marker"

if [ ! -f "$MARKER" ]; then
  echo "No certificate was renewed -- leaving nginx as-is"
  exit 0
fi

echo "Certificate renewed -- reloading nginx"
rm -f "$MARKER"
podman exec sparql-proxy nginx -s reload
