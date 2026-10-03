# Deploying signaling and coturn to a VPS

Goal: the Mac and the phone both make outbound connections to one public
server, so devices that cannot reach each other directly (campus Wi-Fi with
client isolation, cellular NAT) can still connect. The server runs two
things:

- the Go **signaling server**, behind Caddy for HTTPS/WSS
- **coturn**, the TURN relay that carries video when a direct path fails

These steps were written without access to a server and have not been run
end to end. If a command fails, fix it and update this file.

## 1. Create the server

- Any small Ubuntu 24.04 VPS works (1 vCPU, 1 GB RAM). Pick the region
  closest to you, since relayed video passes through it.
- Add your SSH public key when creating it. Note the public IP.
- If the provider has its own firewall, apply the ports in step 3 there too.

### Vultr notes (the provider actually used)

- Create a Shared CPU instance with the smallest plan that has an IPv4
  address (the cheapest IPv6-only plan will not work), Ubuntu 24.04, and turn
  off Auto Backups.
- The default login is `root`, so use `root` for `<user>` and
  `/root/remotebridge` for paths, including in the systemd unit
  (`User=root`). Acceptable for a short test; use a normal user for anything
  long-lived.
- The public IP is attached to the server directly, so leave `external-ip`
  commented out in the coturn config, and `ufw` in step 3 is all the
  firewalling you need.
- Destroy the instance when you are done (Settings, Destroy). Stopping it
  still bills.

### Azure notes (not used: student sizes were unavailable in every region tried)

- Azure for Students gives credit with a `.edu` email and needs no card. The
  policy may limit which regions and sizes you can pick; choose the closest
  allowed region and the smallest B-series size (for example B1s).
- The default admin username is `azureuser`; use it wherever this guide says
  `<user>`.
- Azure filters traffic in a Network Security Group, separate from the
  server's own firewall. Add these inbound rules (VM, Networking, Add inbound
  port rule): TCP 80, TCP 443, TCP 3478, UDP 3478, UDP 49152-49252. Step 3's
  `ufw` is then optional.
- The server sits behind NAT, so set `external-ip=<public>/<private>` in the
  coturn config (step 6). The private IP is the first address printed by
  `hostname -I`.
- Delete the resource group when you are done testing so it stops using
  credit.

## 2. Get a domain name

Caddy needs a domain to get a certificate, and the token should not travel
in clear text. A free subdomain from DuckDNS is enough: sign in at
duckdns.org, create a name, and set its IP to the server's public IP.

## 3. Open the firewall

```
sudo ufw allow OpenSSH
sudo ufw allow 80/tcp          # Caddy (certificate issuance and redirect)
sudo ufw allow 443/tcp         # Caddy (HTTPS and WSS)
sudo ufw allow 3478/tcp        # coturn
sudo ufw allow 3478/udp        # coturn
sudo ufw allow 49152:49252/udp # coturn relay range (min-port/max-port)
sudo ufw enable
```

Port 8080 is deliberately not opened. The signaling server listens on
localhost and only Caddy talks to it.

## 4. Install packages

```
sudo apt update
sudo apt install -y coturn caddy golang-go
```

## 5. Generate two secrets

```
openssl rand -hex 16   # REMOTEBRIDGE_TOKEN: what the host and viewer present
openssl rand -hex 32   # TURN secret: shared by the signaling server and coturn
```

Keep both; you will use each in more than one place.

## 6. Configure coturn

Do this after step 7's `rsync`, or copy the file over yourself. Copy
`deploy/turnserver.conf.example` to `/etc/turnserver.conf`, then set
`static-auth-secret` to the TURN secret. Then:

```
sudo cp ~/remotebridge/deploy/turnserver.conf.example /etc/turnserver.conf
sudo nano /etc/turnserver.conf
sudo systemctl enable coturn
sudo systemctl restart coturn
sudo systemctl status coturn
```

Use `restart`, not `enable --now`. The Debian package starts coturn with its
default config as soon as it is installed, and `enable --now` does not
restart a service that is already running, so the new config would be
silently ignored. That default setup still answers STUN but rejects every
TURN credential, so the symptom is srflx candidates but no relay candidates.
After restarting, `journalctl -u coturn -n 40` should show
`Default realm: remotebridge`; a blank realm means the config was not read.

## 7. Deploy the signaling server

From your laptop, copy the server, the client page, and the deploy files. The
server and client must stay side by side, because the server serves
`../client`:

```
ssh <user>@<server-ip> mkdir -p remotebridge
rsync -av signaling client deploy <user>@<server-ip>:remotebridge/
```

On the server, build it and install the service:

```
cd ~/remotebridge/signaling
go build -o remotebridge-signaling .
sudo cp ~/remotebridge/deploy/remotebridge-signaling.service.example \
  /etc/systemd/system/remotebridge-signaling.service
sudo nano /etc/systemd/system/remotebridge-signaling.service
```

In the file, fill in the user, the paths, the token, the server's public IP
with `:3478` for `REMOTEBRIDGE_TURN_HOST`, and the TURN secret. Then:

```
sudo systemctl daemon-reload
sudo systemctl enable --now remotebridge-signaling
sudo systemctl status remotebridge-signaling
```

The log should say `TURN relay offered at <ip>:3478`. If it says "no TURN
relay configured", the TURN variables are not reaching the process.

## 8. Configure Caddy

Copy `deploy/Caddyfile.example` to `/etc/caddy/Caddyfile`, replace the domain
with yours, then `sudo systemctl reload caddy`. Caddy requests the certificate
on the first request, so DNS must already point at the server.

## 9. Test from your laptop

Viewer page in a browser (should load over HTTPS and show a black video):

```
https://<name>.duckdns.org/?token=<TOKEN>
```

Host on the Mac:

```
export REMOTEBRIDGE_TOKEN=<TOKEN>
cd host
cargo run -- webrtc --server wss://<name>.duckdns.org
```

## 10. Prove the relay works

A direct connection can quietly win and hide a broken relay. Force the relay
on both sides so the stream can only succeed through coturn:

```
cargo run -- webrtc --server wss://<name>.duckdns.org --relay
```

and open `https://<name>.duckdns.org/?token=<TOKEN>&relay=1` on the phone. If
video plays, TURN works. In the coturn log (`sudo journalctl -u coturn`) you
should see allocations for both peers.

## 11. The real test

Mac on the campus Wi-Fi, phone on cellular, both using the server above,
first without `--relay` and then with it.

## If campus Wi-Fi still fails

Some networks block UDP 3478 and even TCP 3478. If the `--relay` test works
from home but not on campus, the next step is serving coturn over TLS on
port 443, which looks like ordinary web traffic. That conflicts with Caddy on
443, so it needs a second IP or a TLS-routing proxy in front of both.
