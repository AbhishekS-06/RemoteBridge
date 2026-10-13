# Process log

A chronological account of how RemoteBridge was built: what was done, why each
decision was made, what went wrong, and what changed as a result. It is the
story behind the code, for looking back on and for explaining the project in
an interview. For individual bugs in detail, see `debugging-log.md`. For the
current state of the project, see the README.

Dates come from the git history (September 2026). Entries after the last
commit are marked as uncommitted.

## Approach

- **Phased plan (v0 to v5), one working thing per phase.** Each phase ends with
  something that runs, so problems are found while the change is small.
- **Explain every part.** This is a portfolio project. The rule was to write
  code that can be defended line by line, keep explanations short and
  concrete, and avoid magic (no CLI framework for two options, no custom
  crypto).
- **Verify before claiming.** Compile and unit-test what can be tested;
  say plainly what has not been run; test on real devices for the rest.

## v0: capture, encode, write a file (Sep 13 to 15)

Goal: prove the media pipeline with no networking.

- **Sep 13:** Created the Rust host crate with `ffmpeg-next`,
  `screencapturekit` and `image`. Captured frames from the main display with
  ScreenCaptureKit. Only the Command Line Tools were installed, not full
  Xcode, so the capture crate could not find the Swift libraries; the fix was
  a library search path and an rpath in `host/.cargo/config.toml`
  (kept deliberately).
- **Sep 14:** Milestone 1, a single frame saved as a PNG. Milestone 2, an H.264
  encoder (libx264) tested on a synthetic gradient before touching real
  capture, so encoder bugs and capture bugs could not hide each other.
  Milestone 3, real capture converted from BGRA to YUV420P with swscale,
  encoded, and written to a file playable after remuxing to MP4.
- **Encoder choices and why:** `veryfast` preset, `zerolatency` tune, no
  B-frames, one keyframe per second, CRF 23. Latency matters more than
  compression for a remote desktop, and a one-second keyframe interval means a
  viewer that joins or loses a packet recovers quickly.
- **Housekeeping:** ignored generated files (`*.h264`, `*.png`, `*.mp4`) and
  the local Claude context file.

## v1: same-network streaming (Sep 16 to 19)

Goal: see the Mac's screen in a phone browser on the same Wi-Fi.

- **Sep 16, signaling server (Go):** a small WebSocket relay with one host slot
  and one viewer slot. It treats messages as opaque bytes, so it stays correct
  if the message shapes change. No rooms yet; one Mac and one phone did not
  justify the complexity, and multi-device is a v4 concern.
- **Sep 19, host and client:** `webrtc-rs` on the host with one outbound H.264
  track, `signaling.rs` for the WebSocket client, and a plain
  `client/index.html` using the browser's own `RTCPeerConnection`. No trickle
  ICE: each side waits for candidate gathering to finish and sends one
  complete SDP, which keeps the protocol to two message types.
- **Result:** the Mac screen appeared in Safari on an iPhone. That was the v1
  goal. In hindsight this was also the only browser engine tested, which
  mattered later (see the DTLS bug in v2).

## v2: working across networks (Sep 20, in progress)

Goal: stream to a phone on a different network, including campus Wi-Fi that
blocks devices from reaching each other.

### The offer race (committed Sep 20)

The host sent its offer once at startup, and the relay dropped messages when no
viewer was connected, so the host had to be started second. That is backwards
from real use. The relay now holds the offer until a viewer answers and
replays it to a late viewer (`pendingOffer`). Cleared on answer or host
disconnect so a new viewer never receives a dead offer. Known leftover:
reloading the viewer after connecting needs a host restart, because the host
reads only one answer.

### README and diagrams (committed Sep 20)

Wrote the README with a phase table, tech stack, run steps, and limitations, and
made keeping it current a standing rule. The first version used Mermaid code
blocks, but GitHub's viewer overlays zoom controls on the diagram, so the
diagrams are now rendered to static PNGs from `.mmd` sources in
`docs/diagrams/`. Text size and layout were checked by looking at the
rendered images; two layouts had to be reworked because Mermaid ignored
`direction TB` when edges connected to nodes inside the boxes. The `*.png`
ignore rule would have kept the images out of git, so an exception was added.

### STUN, token, configurable server (uncommitted at the time of writing)

- **STUN** added on both sides so each peer can learn its public address.
- **Configurable server and a shared token.** The host's `ws://localhost:8080`
  was hardcoded; it is now a flag or environment variable. The server rejects
  connections without the token before they can take a slot (constant-time
  comparison). The token is limited to URL-safe characters so no URL-encoding
  crate was needed, and the host gained TLS support so `wss://` works. Unit
  tests cover the rejection cases, including that a rejected connection cannot
  displace the real viewer.

### TURN with short-lived credentials (uncommitted)

A browser cannot keep a permanent TURN password secret. The signaling server
mints a temporary credential (coturn's shared-secret scheme: username is an
expiry time, credential is the HMAC-SHA1 of it) and sends it to each client
in a `config` message as the first thing after connecting. Both sides wait
for that message before building the peer connection. A unit test recomputes
the HMAC independently to check the server's output matches what coturn
expects. `--relay` and `?relay=1` restrict a connection to relay candidates,
so the relay path can be proven instead of assumed.

### Choosing where to run it

- **Azure for Students:** blocked every small VM size in every region tried.
- **DigitalOcean through the GitHub Student Pack:** no longer offered. I had
  recommended it from memory before checking; a search and the Pack's own page
  showed it had been removed. Verify offers before planning around them.
- **Vultr** worked: shared CPU, 1 vCPU and 1 GB, about 5 dollars a month, in
  Atlanta. The server runs coturn, the Go signaling server behind Caddy for
  HTTPS, and is reached through a free DuckDNS name.

### Getting it running end to end (what actually happened)

1. Server set up from `docs/deploy.md`; coturn, signaling and HTTPS all came up.
2. First host run failed with `ErrTurnCredentials`: a bug in my host code.
3. Then `401`: the token on the laptop did not match the server's; rotated it.
4. Host connected but no relay candidates: coturn had never loaded its config
   because the package starts it before you configure it.
5. Relay candidates appeared, but the handshake failed: the DTLS curve bug.
   It reproduced locally, which ruled out the VPS, and reading the library
   source confirmed the cause. Fixed by vendoring the library with the upstream
   fix.
6. Chrome or Brave on the Mac then worked locally.
7. Relay-only test against the VPS (`--relay` and `?relay=1`) passed: the
   selected candidate pair was relay to relay, both addresses on the Vultr
   server, then DTLS and the peer connection reached `connected`. This proves
   the video path Mac, coturn, browser works with the short-lived credentials.
8. Not yet run: the real test, a phone on cellular against a Mac on campus
   Wi-Fi. The Mac was on a home network at this point. (I briefly recorded
   this as passed after misreading "both tests worked" as covering it, and
   corrected it; the two tests that passed were the direct and relay-only
   runs against the VPS.)
9. Oct 1: first real cross-network test. iPhone on cellular (Wi-Fi off),
   Mac on home Wi-Fi, normal mode (no `--relay`). The phone first sat at
   `ice: checking` with host, srflx and relay candidates all gathered, then
   connected and played video. No code or config changed between the stuck
   view and the working one, so the cause of the wait is unknown; most
   likely ICE was still working through candidate pairs over the carrier's
   NAT. The selected pair (direct or relay) was not captured, so which path
   cellular used is still open. Relay-only over cellular and campus Wi-Fi
   are not yet run.
10. Oct 2: reran on cellular, normal then relay-only. Both connected, then
    dropped with no video, because closing the lid slept the Mac (debugging
    log #7). The normal run's selected pair was Mac host candidate to the
    phone's relay on coturn: cellular could not go direct, so TURN carried
    it. Relay-only connected relay to relay; video still needs a run with
    the lid open.
11. Oct 2: relay-only rerun with the lid open and the phone on cellular.
    Video played. TURN is proven across two real networks. Campus Wi-Fi is
    the only v2 test left, and v3 does not depend on it.

Details and lessons for each are in `debugging-log.md`.

## CI (Oct 2)

Added `.github/workflows/ci.yml` before v3, so the input work has a safety
net. Two jobs: the Go server on Linux (gofmt check, `go vet`, `go test
-race`), and the host on a macOS runner (`brew install ffmpeg`, `cargo
build` and `cargo test`), because ScreenCaptureKit only exists on macOS.
A rustfmt check was left out: the host code is not rustfmt-formatted yet.
The first run failed: I picked the `macos-15` runner without checking which
SDK the dependencies need, and `apple-metal` needs the macOS 26 SDK
(debugging log #8). Moved the job to `macos-26`.
Docker, k3s and automatic deploy wait until after v4, when the signaling
protocol stops changing and a long-lived server exists.

## v3: input from the phone (Oct 3, in progress)

Work now happens on feature branches with a PR so CI checks each change
before `main` (this one: `v3-input`).

1. Data channel first, before any real input. The host creates an `input`
   channel before it makes the offer, so the channel is in the SDP and the
   phone receives it through `ondatachannel` with no renegotiation. Default
   settings (ordered, reliable) because input must not be lost or reordered:
   a dropped "release" would leave a button held down. The phone sends a
   hello on open and a numbered message per tap; the host prints them. This
   proves the path phone to host works, including through TURN, before any
   mouse control is written.
2. Oct 5, campus Wi-Fi test (the case that motivated v2). Ports 3478 and 443
   were open, but the host never got the phone's answer: the viewer waited
   for ICE gathering to complete, and that outlasted ICE's own timeout.
   Capped the wait at 3 s; it then connected on campus (debugging log #9).
   I first guessed a stale host connection; the server log disproved it.
   Reading the log before theorizing would have been faster.
3. Oct 6, docs caught up before the first v3 commit: README status, run
   output and design decisions now describe the input channel and the 3 s
   answer cap; architecture and connection-setup diagrams show the channel.
   A separate reverse arrow for input made Mermaid reshuffle the whole
   architecture layout, so input shares the peer-to-peer edge label instead.
4. Oct 12, input channel test run and passed (Mac browser on localhost,
   same machine as the host; not yet run on the iPhone or through TURN):
   the host printed `input
   channel open`, the hello, and one `input: tap N` per tap, matching the
   count on the viewer page. The path from viewer to host is proven, so the
   next step can build the real input protocol on top of it. My first
   test instructions said "open the viewer" without giving the URLs; the
   second pass listed exact URLs per setup (localhost, LAN IP, VPS).

5. Oct 12, viewer reload. A reloaded page got no video until the host
   restarted: the host made one offer at startup, the server replayed it to
   a late viewer, and the first answer used it up. Replaced the replay with
   a `viewer-joined` message from the server; the host now runs a loop that
   gives every joining viewer a new peer connection and offer, numbered so a
   stale answer (two quick reloads) is ignored. The video track is created
   once and shared, so capture never stops. Side effects: restarting the
   host while the page stays open also works now, and the server got
   simpler (it no longer parses message types). Signaling moved to its own
   tokio task, which forced the error type to `Send + Sync`. Host builds,
   Go tests pass (new test for viewer-joined in both connect orders and on
   reload); not yet run end to end.
6. Oct 12, reload test on the Mac browser passed: six sessions in one host
   run, each reload (including a hard refresh after clearing cookies)
   printed `viewer joined, starting session N` and reconnected. Session 1
   never got an answer; most likely the tab still had the pre-change page,
   whose answer has no `session` field, so the host's parser skipped it
   without a word. Old page plus new host fails silently, which is worth
   remembering when the VPS still serves the old page.
7. Oct 12, PR conflicts after a squash merge. The input channel commit was
   merged to `main` as PR #2 with "Squash and merge", which creates a new
   commit (`e746044`) with identical contents but a different hash. The
   reload work had been committed on top of the original commit on
   `v3-input`, so the next PR showed conflicts in six files: git saw two
   unrelated edits to the same lines, not the same change twice. No content
   actually differed (`git diff 9992836 e746044` was empty). Fixed with
   `git rebase --onto origin/main 9992836 v3-input` and a
   `--force-with-lease` push, which replayed only the reload commit onto
   `main`; the open PR updated in place. Lesson: after a squash merge,
   branch the next work from the updated `main`, or use merge commits.
8. Oct 12, real touch input (branch `v3-input-protocol`). The test taps are
   replaced by pointer events: `{"type":"down"|"move"|"up","x","y"}` with
   x and y as fractions (0..1) of the video frame. One finger is the left
   mouse button, so a tap clicks and a slide drags. Two letterboxes have to
   be undone: the page measures the touch against the area the video is
   drawn in (`object-fit: contain`), and the host removes the side bars
   ScreenCaptureKit adds when it fits the 1512x982-point screen into
   1280x720 (visible in the old `frame.png`). Without the second step a tap
   lands up to ~86 frame pixels off near the screen edges. Injection uses
   `enigo` on one dedicated thread; the data channel callback only parses
   and forwards. When a channel closes the host releases the button, so a
   viewer that drops mid-drag cannot leave it held. Unit tests cover the
   mapping (centre, edges, bars) and JSON parsing. Host builds, tests pass,
   page JS parses; not yet run.
9. Oct 12, touch test on the Mac browser (localhost) worked. The first try
   failed with `ConnectionRefused` because the signaling server was not
   running; the host connects to it before anything else.
10. Oct 12, touch test on the iPhone through the VPS (normal mode) worked:
    tapping and dragging moved and clicked the Mac's cursor. It is awkward
    to use (one finger as an absolute mouse on a small screen), left for
    later. Getting there took one detour: the VPS had to be updated because
    the reload work added a server message (`viewer-joined`), and the first
    attempt left the old binary running, so the phone sat at "connecting to
    signaling..." (debugging log #10). Input itself never touches the
    signaling server; it rides the data channel, so for touch alone only
    the page needed copying.
11. Oct 12, touch merged to main as PR #4 (a400f3d). Relay-only touch test
    with the Mac and the iPhone both on campus Wi-Fi (`--relay` and
    `?relay=1`) worked: tap clicks, and drag moves windows and selects
    text. This also covers the campus relay-only run that was still open
    from Oct 5. The first host start failed with `ConnectionRefused` on
    port 443 to the VPS; a moment later DNS, port 443 and the page all
    answered from the same Mac, so it was a passing network refusal, not
    the code. Two-finger tap and drag did nothing, as built: the page
    sends only the primary pointer and the host only presses the left
    button.
12. Oct 12, reload test on the iPhone through the VPS (normal mode)
    worked: each reload brought the video back with a new host session.
13. Oct 12, trackpad mode written on branch `v3-trackpad`. Pointing
    directly with one finger was awkward on the phone, so touch now acts
    as a trackpad: one finger moves the cursor relatively, a tap clicks,
    tap-then-slide drags, a two-finger tap right-clicks, and two fingers
    sliding scroll. The page recognises the gestures (it has the timing and
    the finger count) and sends new messages: `move_rel`, `click`, `press`,
    `release`, `scroll`, with distances as frame fractions like positions.
    A mouse on a desktop browser keeps the old direct down/move/up. Host
    details: the injector now remembers which buttons it holds (Reset
    releases all of them), relative moves are clamped to the display by the
    host because enigo's own relative move is not, and leftover fractions
    of a point carry over so slow movement does not stall. Scroll uses
    enigo's pixel scrolling, which needs its `platform_specific` feature.
    Host builds with no warnings, 6 unit tests pass, and a Node harness fed
    fake touch events to the page code and checked the messages for each
    gesture. Not yet run on the iPhone; the scroll direction in particular
    needs checking against the Mac's natural scrolling setting.
14. Oct 12, trackpad mode tested on the iPhone through the VPS (normal
    mode): all the gestures worked. Deploying it only needed the page
    copied with `rsync`; the server's Go code did not change.

## Mistakes worth remembering

- Recommending a provider offer from memory without checking it was current.
- Telling the reader to `export` a variable "in the same shell" when the server
  and host run in two different terminals.
- Using `systemctl enable --now` for a service that a package had already
  started, so a config change was silently ignored. The deploy guide now says
  `restart`.
- Shipping the TURN config with library defaults that the library rejects,
  instead of reading how it validates credentials.
- Calling something "working" after testing only one browser engine.
- Staging and committing the v2 work myself when asked "should we just
  commit and move on". Git (add, commit, push) is the user's alone; I undid
  the commit with `git reset HEAD~1` before it was pushed.
- Long instruction blocks when the reader was mid-way through configuring a
  cloud console. Smaller steps, one at a time, work better.
- Starting the touch-input code (adding a dependency, writing files) while
  the user was still on `main`, before they had made a branch. Nothing was
  committed, and `git switch -c` carried the changes to the new branch, but
  branching is the user's step and comes first.
- The viewer page's status text says "connecting to signaling..." until an
  offer arrives, and the page has no WebSocket open/close handlers. So a
  connected page waiting for the host looks exactly like one that never
  reached the server, which pointed the first guess at the wrong layer.
- Reporting touch input as "not yet merged" after comparing against a local
  `main` that had not been fetched since PR #4 merged. Fetch first, then
  compare against `origin/main`.
- Describing the planned trackpad gestures in a table without saying clearly
  enough that none of them were built, so two-finger right click and scroll
  were tested as if they already existed.

## What I would do differently

- Test with at least two browser engines (WebKit and Chromium) from the first
  streaming milestone.
- Add ICE and DTLS logging as soon as WebRTC is introduced, not after the first
  failure.
- Verify each cloud provider's offer and the account's limits before writing
  deployment steps for it.
- Keep a written record as the work happens, which this file now does.

## Still to do

- Keyboard input.
- Destroy the test server and rotate the token when finished.
- v3: input forwarding. v4: pairing and multi-device. v5: native client,
  Windows and Linux hosts.
