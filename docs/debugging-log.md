# Debugging log

Problems hit while building RemoteBridge, how each was diagnosed, and what
fixed it. Newest first within each phase. The goal is to be able to explain
not just the fix but how the cause was found.

## Method that worked

1. **Read the exact error before theorizing.** Most of the entries below were
   settled by one log line.
2. **Add logging where the evidence is missing.** ICE candidate types, the
   selected candidate pair, DTLS state, and `RUST_LOG=webrtc_dtls=debug`
   turned "it fails" into "it fails here, for this reason".
3. **Remove variables.** The DTLS bug looked like a VPS or relay problem. Running
   the same code against a local server showed it was not.
4. **Confirm a suspected cause in the source before fixing it.** For the DTLS
   bug, the vulnerable line was found in the locked library version and the fix
   in a newer one.
5. **Check reachability layer by layer**: DNS (`dig`), port (`nc -vz`), service
   (`systemctl`, `journalctl`), then application.

Useful commands:

```
dig +short <name> @8.8.8.8                 # does the name resolve, and to what
nc -vz -w 5 <ip> <port>                    # is the port reachable
curl -sI https://<name>/                   # does HTTPS answer
journalctl -u <service> -n 40 --no-pager   # a service's recent log
ss -lunpt | grep <port>                    # what is listening on a port
RUST_LOG=webrtc_dtls=debug,webrtc=debug cargo run -- webrtc ...   # library logs
```

## v2: cross-network streaming

### 1. Chrome and Brave fail with "invalid named curve"

- **Symptom:** ICE reached `connected`, then `dtls state: failed` and
  `peer connection state: failed`. The browser's status box said
  `connection: failed`, `ice: connected`. Happened on localhost too, with no
  VPS involved. The iPhone had worked all along.
- **Diagnosis:** The debug log showed the browser's ClientHello being parsed
  (TLS 1.3 extensions reported as unsupported) and then
  `Failed to start manager dtls: invalid named curve`. A search found
  webrtc-rs issue #417. Reading the locked `webrtc-dtls` 0.10.0 source showed
  `state.named_curve = e.elliptic_curves[0];`, and 0.12.0 showed a loop that
  takes the first supported curve.
- **Cause:** The library takes the first curve the browser lists and aborts if it
  does not implement it. Chromium browsers now list a newer curve first.
  Safari lists a known one first, which is why only the iPhone connected.
- **Fix:** Vendored `webrtc-dtls` 0.10.0 into `host/vendor/webrtc-dtls`, applied
  the upstream loop, and wired it in with `[patch.crates-io]` in
  `host/Cargo.toml`. Remove both when `webrtc` is upgraded past the fix.
- **Lesson:** "It connected on one device" is not evidence it works on others.
  A failure that reproduces locally is not a network problem.

### 2. Relay works for STUN but gives no relay candidates

- **Symptom:** Host and browser both gathered host and srflx candidates but no
  `relay` candidate. The server log said `TURN relay offered`.
- **Diagnosis:** coturn was active and listening on 3478, and the config file
  looked right, but the log showed `Default realm:` empty and a start time of
  01:18, before the config was copied at about 01:22.
- **Cause:** The Debian package starts coturn with default settings as soon as
  it is installed. `systemctl enable --now coturn` does not restart a service
  that is already running, so the new config was never loaded. The default
  setup answers STUN but rejects TURN credentials.
- **Fix:** `systemctl restart coturn`. The deploy guide now says to use
  `restart`, and why.
- **Lesson:** Compare the service's start time against the config's change time.

### 3. `Error: ErrTurnCredentials` on the host

- **Symptom:** The host exited right after connecting to the signaling server.
- **Diagnosis:** Read the webrtc-rs source for that error.
- **Cause:** Building `RTCIceServer` with `..Default::default()` left the
  credential type as `Unspecified`, which the library rejects for any `turn:`
  URL. Browsers assume a password, so only the host was affected.
- **Fix:** Set `credential_type: RTCIceCredentialType::Password` explicitly.

### 4. `401 unauthorized` from the signaling server

- **Symptom:** The host connected to the VPS and got HTTP 401.
- **Cause:** The token exported in the terminal did not match the token in the
  server's service file. Environment variables only exist in the terminal
  window where they were exported, and it is easy to run the host from a
  different one.
- **Diagnosis:** `echo ${#REMOTEBRIDGE_TOKEN}` prints the length without
  revealing the value; the server log printed `rejected ... bad or missing
  token`.
- **Fix:** Generate a new token on the server and export the same value on the
  laptop. This also replaced a token that had been pasted into a chat.

### 5. DuckDNS name did not resolve

- **Symptom:** `dig` and `nslookup` returned `NXDOMAIN`.
- **Cause:** The name used in the commands was misspelled compared to the
  registered one (`remotebridgdev` versus `remotebridgedev`).
- **Fix:** Read the name from the DuckDNS dashboard and confirm with `dig`
  before using it anywhere.

### 6. HTTPS check on the server came back empty

- **Symptom:** `curl -sI https://<name>/` printed nothing right after starting
  Caddy.
- **Cause:** Caddy was still obtaining its certificate. Testing again from
  another machine a minute later returned `HTTP/2 200`.

### 7. Connected over cellular, then dropped with no video

- **Symptom:** iPhone on cellular, two runs (normal and `--relay`). Both
  reached ICE, DTLS and peer connection `connected`, then went
  `disconnected` (and `failed` in the first). No video on the phone. The
  first run also ended with `SCStream error (No capture source provided):
  Failed to find any displays or windows to capture`.
- **Diagnosis:** The capture error means macOS had removed the display,
  which points at sleep. `pmset -g log` showed `Clamshell Sleep` at
  19:03:29 and 19:07:25, each right after a lid-open wake, matching the two
  runs. The lid was closed on battery with no external display, so the whole
  Mac slept: Wi-Fi stopped, ICE keepalives went unanswered, and on wake
  ScreenCaptureKit had no display.
- **Cause:** Test procedure, not code.
- **Fix:** Keep the lid open (and ideally plugged in) while hosting.
- **Lesson:** When a connection that worked drops on its own, check the
  machine's power and sleep log before debugging the network. A host that
  sleeps looks exactly like a network failure from the other side.
- **Side result:** The normal run's selected pair was Mac host candidate to
  the phone's relay candidate on coturn, so cellular could not go direct and
  TURN carried the stream. The relay-only run connected relay to relay.

## Provider choice (not a bug, but it cost time)

- **Azure for Students:** every small VM size returned
  `NotAvailableForSubscription` in every region tried, so it could not be used.
- **DigitalOcean via the GitHub Student Pack:** no longer offered; the credits
  ended in August 2026.
- **Vultr** shared CPU, `vc2-1c-1gb`, about 5 dollars a month, worked.
  Avoid the cheapest plan if its name ends in `-v6`, which is IPv6 only.
- **Lesson:** check that an offer still exists and that the account can use the
  resource before building a plan around it.

## Earlier

### Viewer had to connect before the host

- **Symptom:** If the host started first, its offer was dropped and it waited
  forever.
- **Fix:** The signaling server holds the host's offer until a viewer answers
  and replays it to a late viewer (`pendingOffer` in `signaling/main.go`).
