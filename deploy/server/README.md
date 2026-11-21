# VmaX regional signaling and TURN

This stack serves VmaX **Auto** connections. Use an Ubuntu VPS close to the players, such as Mumbai or Hyderabad for players in India. A nearby relay can reduce WAN RTT when a direct path is unavailable. Measure from each ISP before choosing a region if you have multiple options. Availability, transfer allowance, and egress charges depend on the VPS plan.

| Public port | Purpose |
| --- | --- |
| TCP 22 | SSH (substitute your SSH port if different) |
| TCP 80 | Let's Encrypt HTTP-01 issuance and renewal |
| TCP 443 | WSS signaling and TURN over TLS, selected by TLS hostname |
| UDP 3478 | STUN binding and TURN allocation |
| UDP 49160–49259 | TURN relay traffic |

## Set up the VPS

1. Install Docker Engine with the Compose plugin, Certbot, and UFW. Reserve a public IPv4 address. Point two DNS-only A records, such as `signal.example.net` and `turn.example.net`, at that address. Use distinct names so HAProxy can route the TLS connections. This deployment listens publicly on IPv4. Peers can still exchange and probe their own IPv6 addresses.
2. Add the same ingress rules to the provider firewall or OCI network security group. Allow return traffic and outbound UDP to Internet peers. Keep TCP 5349, 8443, and 8788 private. On the VPS run these commands before starting the stack.

   ```sh
   sudo ufw allow 22/tcp
   sudo ufw allow 80/tcp
   sudo ufw allow 443/tcp
   sudo ufw allow 3478/udp
   sudo ufw allow 49160:49259/udp
   sudo ufw default deny incoming
   sudo ufw default allow outgoing
   sudo ufw --force enable
   sudo ufw status verbose
   ```

3. From the VmaX checkout, create the configuration and secrets. These commands use rootful Docker. Keep the secret and certificate files owned by root with mode 600 so the containers can read them with their restricted capabilities.

   ```sh
   cd deploy/server
   cp -n .env.example .env
   chmod 600 .env
   editor .env
   sudo install -d -m 700 secrets certs
   sudo sh -c 'umask 077; set -C; openssl rand -hex 32 >secrets/turn-secret'
   sudo sh -c 'umask 077; set -C; openssl rand -hex 32 >secrets/regional-host-token'
   ```

   Set `VMAX_PUBLIC_IPV4` to the reserved public address and `VMAX_PRIVATE_IPV4` to the address shown by `ip -4 addr` on the VM interface. Set them to the same value if the public address is assigned directly to that interface. Keep the external and internal relay port numbers equal if the VPS uses NAT.

4. Issue one certificate containing both hostnames, then start the stack. Replace both example names with the values from `.env`.

   ```sh
   sudo certbot certonly --standalone --cert-name vmax-regional \
     -d signal.example.net -d turn.example.net
   sudo env VMAX_SKIP_CERT_RESTART=1 sh ./sync-certificates.sh \
     /etc/letsencrypt/live/vmax-regional
   sudo docker compose up -d --build
   sudo docker compose ps
   curl --fail https://signal.example.net/healthz
   openssl s_client -connect turn.example.net:443 -servername turn.example.net \
     -verify_hostname turn.example.net -verify_return_error </dev/null
   ```

5. Install a renewal hook. Set `DEPLOY_DIR` to the absolute location of this folder on the VPS.

   ```sh
   DEPLOY_DIR=/opt/VmaX/deploy/server
   sudo tee /etc/letsencrypt/renewal-hooks/deploy/vmax-regional >/dev/null <<EOF
   #!/bin/sh
   set -eu
   [ "\${RENEWED_LINEAGE##*/}" = vmax-regional ] || exit 0
   exec /bin/sh "$DEPLOY_DIR/sync-certificates.sh"
   EOF
   sudo chmod 755 /etc/letsencrypt/renewal-hooks/deploy/vmax-regional
   sudo certbot renew --dry-run
   ```

   The hook restarts ingress and coturn to load the new certificate. The restart can interrupt TURN allocations and expire live room codes. Start hosting again to get a new room code after maintenance.

## Connect VmaX

1. On the streaming host, configure `VMAX_REGIONAL_HOST_TOKEN` with the value in `secrets/regional-host-token` before launching VmaX. Keep this token limited to trusted hosts. Keep `secrets/turn-secret` on the VPS.
2. In VmaX, select **Auto** transport and set **Relay address** under **Host options** to `wss://signal.example.net/v1`. Use **Test relay** to check signaling reachability. Start hosting and share the room code or the full VmaX invite. Viewers using a short code enter the same relay address under **Network options**. The full invite can discover the regional endpoint during admission.
3. Watch the HUD for **LAN Raw UDP**, **Direct P2P**, or **Relay** and compare RTT from at least one LAN client and one client on a different ISP. The hub exchanges encrypted probe addresses and WebRTC setup messages, then closes the per-player signaling socket. The host keeps a separate room registration alive for new players. WebRTC tries direct routes and may use TURN. VmaX retains Iroh for admission and fallback.

TURN credentials are HMAC-authenticated, per-peer, and issued when a viewer is admitted. By default they expire after four hours; set `VMAX_TURN_TTL_SECONDS` between 900 and 21600 to suit your sessions. Long-running relay sessions may need to reconnect when credentials expire. To check UDP from a separate Linux machine with coturn utilities installed, run `turnutils_stunclient -p 3478 turn.example.net`. Complete a relayed VmaX test from a second network to validate the allocation and forwarding path. On VPS providers that map a public IP to a private interface, verify peer traffic between two allocations on the same relay as part of deployment.
