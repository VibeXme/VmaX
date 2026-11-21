#!/bin/sh
set -eu
umask 077

cd "$(dirname "$0")"
lineage=${RENEWED_LINEAGE:-${1:?pass a Certbot certificate directory}}
[ -r "$lineage/fullchain.pem" ] && [ -r "$lineage/privkey.pem" ]
mkdir -p certs
stage=$(mktemp -d certs/.stage.XXXXXX)
trap 'rm -rf "$stage"' EXIT HUP INT TERM
install -m 600 "$lineage/fullchain.pem" "$stage/fullchain.pem"
install -m 600 "$lineage/privkey.pem" "$stage/privkey.pem"
cat "$stage/fullchain.pem" "$stage/privkey.pem" >"$stage/haproxy.pem"
for name in fullchain.pem privkey.pem haproxy.pem; do
  mv -f "$stage/$name" "certs/$name"
done
if [ "${VMAX_SKIP_CERT_RESTART:-0}" != 1 ]; then
  docker compose restart coturn ingress
fi
