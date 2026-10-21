//! Host side of the WebRTC handshake: build a peer connection with one
//! outbound H.264 video track, then drive offer/answer through the
//! signaling server. No trickle ICE (see signaling.rs) -- for v1's
//! same-LAN setup, waiting for gathering to finish before sending each SDP
//! keeps the protocol to two message types instead of three.
//!
//! Every time a viewer joins (including a page reload), the host starts a
//! new session: a fresh peer connection and a fresh offer. The video track
//! outlives sessions, so the capture loop never has to know about them.
//!
//! An answer is only applied if it comes from a paired device (see
//! pairing.rs). Otherwise the host prints the pairing code and waits for
//! the user to approve it on stdin; until then the peer connection has no
//! remote description, so nothing flows.

use std::sync::Arc;

use webrtc::api::APIBuilder;
use webrtc::api::interceptor_registry::register_default_interceptors;
use webrtc::api::media_engine::{MIME_TYPE_H264, MediaEngine};
use webrtc::data_channel::data_channel_message::DataChannelMessage;
use webrtc::ice_transport::ice_connection_state::RTCIceConnectionState;
use webrtc::ice_transport::ice_credential_type::RTCIceCredentialType;
use webrtc::ice_transport::ice_server::RTCIceServer;
use webrtc::interceptor::registry::Registry;
use webrtc::peer_connection::RTCPeerConnection;
use webrtc::peer_connection::certificate::RTCCertificate;
use webrtc::peer_connection::configuration::RTCConfiguration;
use webrtc::peer_connection::policy::ice_transport_policy::RTCIceTransportPolicy;
use webrtc::peer_connection::sdp::session_description::RTCSessionDescription;
use webrtc::rtp_transceiver::rtp_codec::RTCRtpCodecCapability;
use webrtc::track::track_local::TrackLocal;
use webrtc::track::track_local::track_local_static_sample::TrackLocalStaticSample;

use tokio::io::AsyncBufReadExt;

use crate::input::{InputEvent, InputSender};
use crate::pairing::{self, PairedDevices};
use crate::signaling::{BoxError, IceServerConfig, SignalMessage, SignalingClient};

/// The one video track every session sends. A track can be bound to
/// several peer connections and unbinds itself when one closes, so the
/// capture loop writes here forever and whichever viewer is connected gets
/// the frames. Before the first viewer, writes go nowhere.
pub fn new_video_track() -> Arc<TrackLocalStaticSample> {
    Arc::new(TrackLocalStaticSample::new(
        RTCRtpCodecCapability {
            mime_type: MIME_TYPE_H264.to_owned(),
            ..Default::default()
        },
        "video".to_owned(),
        "remotebridge".to_owned(),
    ))
}

/// Runs for the life of the host: waits for viewers and gives each one a
/// fresh session. Returns only when the signaling connection fails.
///
/// `force_relay` restricts the connection to TURN-relayed candidates only.
/// It exists to prove the relay path works: with it on, a stream can only
/// succeed by going through coturn, so a direct connection can't mask a
/// broken relay.
pub async fn run_sessions(
    mut signaling: SignalingClient,
    force_relay: bool,
    video_track: Arc<TrackLocalStaticSample>,
    input: InputSender,
    certificate: RTCCertificate,
    mut paired: PairedDevices,
) -> Result<(), BoxError> {
    let host_fp = pairing::certificate_fingerprint(&certificate)?;
    println!("host fingerprint: {host_fp}");
    // Lines typed into the host's terminal, for approving new devices. A
    // task reads them so the loop below can wait on stdin and signaling at
    // once.
    let (line_tx, mut lines) = tokio::sync::mpsc::unbounded_channel::<String>();
    tokio::spawn(async move {
        let mut stdin = tokio::io::BufReader::new(tokio::io::stdin()).lines();
        while let Ok(Some(line)) = stdin.next_line().await {
            if line_tx.send(line).is_err() {
                break;
            }
        }
    });

    // The server sends its ICE servers first, before anything else. Wait for
    // them here because every peer connection needs them at construction.
    let server_ice = loop {
        if let SignalMessage::Config { ice_servers } = signaling.recv().await? {
            break ice_servers;
        }
    };

    // Each offer carries a session number and the viewer echoes it in its
    // answer. If a viewer reloads twice quickly, the host can be holding a
    // newer offer when the answer to an older one arrives; the number is how
    // that stale answer is recognised and dropped instead of being applied
    // to the wrong peer connection.
    let mut session: u32 = 0;
    let mut current: Option<Arc<RTCPeerConnection>> = None;
    // An answer from an unpaired device, held until the user decides:
    // (session, answer SDP, viewer fingerprint).
    let mut pending: Option<(u32, String, String)> = None;
    println!("waiting for a viewer...");
    loop {
        let message = tokio::select! {
            message = signaling.recv() => message?,
            Some(line) = lines.recv() => {
                let Some((pending_session, sdp, viewer_fp)) = pending.take() else {
                    continue;
                };
                // A newer viewer replaced this one while the prompt was up.
                let Some(pc) = current.as_ref().filter(|_| pending_session == session) else {
                    continue;
                };
                if line.trim().eq_ignore_ascii_case("y") {
                    paired.add(&viewer_fp)?;
                    println!("device paired");
                    apply_answer(pc, sdp, session).await;
                } else {
                    println!("device rejected");
                    let _ = pc.close().await;
                    current = None;
                }
                continue;
            }
        };
        match message {
            SignalMessage::ViewerJoined => {
                pending = None;
                // The old viewer is gone (the server keeps one viewer), so
                // close its connection now instead of waiting ~30 s for ICE
                // to time out. Closing also unbinds the video track and
                // releases the TURN allocation.
                if let Some(old) = current.take() {
                    let _ = old.close().await;
                }
                session += 1;
                println!("viewer joined, starting session {session}");
                match start_session(
                    &mut signaling,
                    &server_ice,
                    force_relay,
                    &video_track,
                    &input,
                    &certificate,
                    session,
                )
                .await
                {
                    Ok(pc) => current = Some(pc),
                    Err(e) => println!("session {session} failed to start: {e}"),
                }
            }
            SignalMessage::Answer {
                sdp,
                session: answered,
            } if answered == session => {
                let Some(pc) = &current else { continue };
                let Some(viewer_fp) = pairing::fingerprint_from_sdp(&sdp) else {
                    println!("session {session}: answer has no usable fingerprint");
                    continue;
                };
                if paired.contains(&viewer_fp) {
                    println!("session {session}: paired device");
                    apply_answer(pc, sdp, session).await;
                } else {
                    println!(
                        "session {session}: new device, pairing code {}",
                        pairing::pairing_code(&host_fp, &viewer_fp)
                    );
                    println!("check it matches the code on the device, then approve? [y/N]");
                    pending = Some((session, sdp, viewer_fp));
                }
            }
            SignalMessage::Answer {
                session: answered, ..
            } => {
                println!("ignoring answer for old session {answered}");
            }
            // Config was already consumed above, and offers only go from
            // host to viewer, so neither is expected here.
            SignalMessage::Config { .. } | SignalMessage::Offer { .. } => {}
        }
    }
}

async fn apply_answer(pc: &RTCPeerConnection, sdp: String, session: u32) {
    let result = match RTCSessionDescription::answer(sdp) {
        Ok(answer) => pc.set_remote_description(answer).await,
        Err(e) => Err(e),
    };
    if let Err(e) = result {
        println!("session {session}: bad answer: {e}");
    }
}

/// Builds one session's peer connection and sends its offer. The answer is
/// not awaited here: it arrives later through `run_sessions`, which may see
/// another viewer join first.
async fn start_session(
    signaling: &mut SignalingClient,
    server_ice: &[IceServerConfig],
    force_relay: bool,
    video_track: &Arc<TrackLocalStaticSample>,
    input: &InputSender,
    certificate: &RTCCertificate,
    session: u32,
) -> Result<Arc<RTCPeerConnection>, BoxError> {
    let mut media_engine = MediaEngine::default();
    media_engine.register_default_codecs()?;

    let mut registry = Registry::new();
    registry = register_default_interceptors(registry, &mut media_engine)?;

    let api = APIBuilder::new()
        .with_media_engine(media_engine)
        .with_interceptor_registry(registry)
        .build();

    // STUN lets each side learn its public address and offer it as an extra
    // (srflx) candidate, so two peers on different networks can try to reach
    // each other directly. It only answers "what address do you see me as?"
    // -- it never carries video. TURN (from the server's config message) is
    // the fallback for networks that block the direct path: the video is
    // relayed through coturn instead.
    let mut ice_servers = vec![RTCIceServer {
        urls: vec!["stun:stun.l.google.com:19302".to_owned()],
        ..Default::default()
    }];
    ice_servers.extend(server_ice.iter().map(|server| RTCIceServer {
        urls: server.urls.clone(),
        username: server.username.clone().unwrap_or_default(),
        credential: server.credential.clone().unwrap_or_default(),
        // webrtc-rs rejects a turn: URL unless this is explicitly Password;
        // the default (Unspecified) fails with ErrTurnCredentials before any
        // network traffic. Browsers assume Password, so the page needs no
        // equivalent.
        credential_type: RTCIceCredentialType::Password,
    }));
    let config = RTCConfiguration {
        ice_servers,
        ice_transport_policy: if force_relay {
            RTCIceTransportPolicy::Relay
        } else {
            RTCIceTransportPolicy::All
        },
        // The saved certificate, so paired viewers see the same fingerprint
        // every session instead of a fresh one per connection.
        certificates: vec![certificate.clone()],
        ..Default::default()
    };
    let peer_connection = Arc::new(api.new_peer_connection(config).await?);

    peer_connection
        .add_track(Arc::clone(video_track) as Arc<dyn TrackLocal + Send + Sync>)
        .await?;

    // Input from the viewer arrives on this data channel. The host creates it
    // because the host makes the offer: a channel created before the offer is
    // described in the SDP, so the viewer receives it via ondatachannel and
    // no renegotiation is needed. Default settings mean ordered and reliable
    // (SCTP over the same DTLS connection as the video), which input needs:
    // a lost or reordered "release" would leave a button stuck down.
    let input_channel = peer_connection.create_data_channel("input", None).await?;
    input_channel.on_open(Box::new(|| {
        println!("input channel open");
        Box::pin(async {})
    }));
    // Each message is one input event as JSON (see input.rs). It is handed
    // to the injector thread rather than injected here, so this callback
    // never waits on the OS. A send only fails when input is disabled.
    let to_injector = input.clone();
    input_channel.on_message(Box::new(move |msg: DataChannelMessage| {
        match serde_json::from_slice::<InputEvent>(&msg.data) {
            Ok(event) => {
                let _ = to_injector.send(event);
            }
            Err(e) => println!("bad input message: {e}"),
        }
        Box::pin(async {})
    }));
    let to_injector = input.clone();
    input_channel.on_close(Box::new(move || {
        println!("input channel closed");
        let _ = to_injector.send(InputEvent::Reset);
        Box::pin(async {})
    }));

    // Diagnostics: which kinds of candidate were gathered (host, srflx from
    // STUN, relay from TURN) and how ICE progresses. Only type and protocol
    // are printed, not addresses. If no relay candidate ever shows up, the
    // TURN server was not reached or rejected the credentials.
    peer_connection.on_ice_candidate(Box::new(|candidate| {
        if let Some(c) = candidate {
            println!("gathered {} candidate ({})", c.typ, c.protocol);
        }
        Box::pin(async {})
    }));
    // When ICE connects, also print which candidate pair it chose (host to
    // host means a direct LAN path, relay means through coturn). A weak
    // reference avoids the connection keeping itself alive through its own
    // callback.
    let pc_weak = Arc::downgrade(&peer_connection);
    peer_connection.on_ice_connection_state_change(Box::new(move |state| {
        println!("ice connection state: {state}");
        let pc_weak = pc_weak.clone();
        Box::pin(async move {
            if state == RTCIceConnectionState::Connected
                && let Some(pc) = pc_weak.upgrade()
            {
                let dtls = pc.sctp().transport();
                let ice = dtls.ice_transport();
                if let Some(pair) = ice.get_selected_candidate_pair().await {
                    println!("selected pair: {pair}");
                }
            }
        })
    }));
    // DTLS is the encryption handshake that runs after ICE connects. When ICE
    // is connected but the peer connection fails, this shows whether the
    // handshake is what failed.
    peer_connection
        .sctp()
        .transport()
        .on_state_change(Box::new(|state| {
            println!("dtls state: {state}");
            Box::pin(async {})
        }));

    peer_connection.on_peer_connection_state_change(Box::new(|state| {
        println!("peer connection state: {state}");
        Box::pin(async {})
    }));

    let offer = peer_connection.create_offer(None).await?;
    let mut gathering_complete = peer_connection.gathering_complete_promise().await;
    peer_connection.set_local_description(offer).await?;
    let _ = gathering_complete.recv().await;

    let local_desc = peer_connection
        .local_description()
        .await
        .ok_or("no local description after ICE gathering completed")?;
    signaling
        .send(&SignalMessage::Offer {
            sdp: local_desc.sdp,
            session,
        })
        .await?;
    println!("offer sent, waiting for viewer's answer...");

    Ok(peer_connection)
}
