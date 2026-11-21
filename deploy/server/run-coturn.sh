#!/bin/sh
set -eu
umask 077

turn_secret=$(tr -d '\r\n' </run/secrets/turn_secret)
case "$turn_secret" in
  *[!a-fA-F0-9]*|"") printf '%s\n' "TURN secret must be hexadecimal" >&2; exit 1 ;;
esac
[ "${#turn_secret}" -eq 64 ] || { printf '%s\n' "TURN secret must have 64 hex digits" >&2; exit 1; }
case "$VMAX_TURN_DOMAIN" in
  *[!a-zA-Z0-9.-]*|"") printf '%s\n' "Invalid TURN domain" >&2; exit 1 ;;
esac
for ip in "$VMAX_PUBLIC_IPV4" "$VMAX_PRIVATE_IPV4"; do
  case "$ip" in
    *[!0-9.]*|"") printf '%s\n' "IPv4 addresses are required" >&2; exit 1 ;;
  esac
done

mapping=$VMAX_PUBLIC_IPV4/$VMAX_PRIVATE_IPV4
if [ "$VMAX_PUBLIC_IPV4" = "$VMAX_PRIVATE_IPV4" ]; then
  mapping=$VMAX_PUBLIC_IPV4
fi
{
  cat /etc/vmax/turnserver.conf
  printf '\nrealm=%s\n' "$VMAX_TURN_DOMAIN"
  printf 'server-name=%s\n' "$VMAX_TURN_DOMAIN"
  printf 'listening-ip=%s\n' "$VMAX_PRIVATE_IPV4"
  if [ "$VMAX_PRIVATE_IPV4" != "127.0.0.1" ]; then
    printf '%s\n' 'listening-ip=127.0.0.1'
  fi
  printf 'relay-ip=%s\nexternal-ip=%s\n' "$VMAX_PRIVATE_IPV4" "$mapping"
  printf 'static-auth-secret=%s\n' "$turn_secret"
} >/run/vmax/turnserver.conf
unset turn_secret
exec turnserver -c /run/vmax/turnserver.conf
