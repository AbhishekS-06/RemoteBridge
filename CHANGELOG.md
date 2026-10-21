# Changelog

Versions follow the project phases: v0.N.0 is the end of phase vN.
Earlier phases (v0 to v2) were finished before releases were tagged, so
they are listed here without tags.

## [0.3.0] - 2026-10-21

Input from the phone to the Mac over a WebRTC data channel.

### Added
- Input data channel between the viewer page and the host.
- Touch input: the viewer sends down/move/up as fractions of the video
  frame; the host maps them to screen points (undoing the capture
  letterbox) and injects them with enigo. Needs the Accessibility
  permission for the terminal running the host.
- Trackpad mode for touch: relative cursor, tap to click, tap-then-slide
  to drag, two-finger tap for right click, two-finger slide to scroll.
  A mouse still points directly.
- Keyboard: typed text and named keys with modifiers (Cmd+C, Cmd+Tab).
  A `kbd` button brings up the phone's on-screen keyboard.
- Viewer reload rejoins: the server sends `viewer-joined` and the host
  starts a fresh session for each viewer.
- CI: rustfmt check, clippy with warnings as errors, govulncheck for the
  signaling server, and cancelling superseded runs.

### Changed
- The viewer waits at most 3 s for ICE gathering before answering, so
  slow networks no longer stall the connection.

## v2 - 2026-10-02 (not tagged)

- Works across networks: STUN, plus a self-hosted coturn TURN server with
  short-lived HMAC credentials sent by the signaling server.
- Signaling address and a shared token are configurable; wss:// supported.
- Fixed Chrome and Brave failing the DTLS handshake (vendored
  webrtc-dtls patch).

## v1 - 2026-09-19 (not tagged)

- Go signaling server and webrtc-rs host stream the Mac screen to a
  browser on the same LAN.

## v0 - 2026-09-14 (not tagged)

- Screen capture with ScreenCaptureKit, encoded to an H.264 file with
  libx264.

[0.3.0]: https://github.com/AbhishekS-06/RemoteBridge/releases/tag/v0.3.0
