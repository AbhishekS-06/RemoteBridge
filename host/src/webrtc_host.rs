//! Host side of the WebRTC handshake: build a peer connection with one
//! outbound H.264 video track, then drive offer/answer through the
//! signaling server. No trickle ICE (see signaling.rs) -- for v1's
//! same-LAN setup, waiting for gathering to finish before sending each SDP
//! keeps the protocol to two message types instead of three.

use std::sync::Arc;

use webrtc::api::APIBuilder;
use webrtc::api::interceptor_registry::register_default_interceptors;
use webrtc::api::media_engine::{MIME_TYPE_H264, MediaEngine};
use webrtc::interceptor::registry::Registry;
use webrtc::peer_connection::RTCPeerConnection;
use webrtc::peer_connection::configuration::RTCConfiguration;
use webrtc::peer_connection::sdp::session_description::RTCSessionDescription;
use webrtc::rtp_transceiver::rtp_codec::RTCRtpCodecCapability;
use webrtc::track::track_local::TrackLocal;
use webrtc::track::track_local::track_local_static_sample::TrackLocalStaticSample;

use crate::signaling::{SignalMessage, SignalingClient};

pub async fn connect_host(
    signaling: &mut SignalingClient,
) -> Result<(Arc<RTCPeerConnection>, Arc<TrackLocalStaticSample>), Box<dyn std::error::Error>> {
    let mut media_engine = MediaEngine::default();
    media_engine.register_default_codecs()?;

    let mut registry = Registry::new();
    registry = register_default_interceptors(registry, &mut media_engine)?;

    let api = APIBuilder::new()
        .with_media_engine(media_engine)
        .with_interceptor_registry(registry)
        .build();

    // No STUN/TURN yet -- v1 is same-LAN only, so every candidate is a host
    // candidate. coturn arrives in v2.
    let peer_connection = Arc::new(api.new_peer_connection(RTCConfiguration::default()).await?);

    let video_track = Arc::new(TrackLocalStaticSample::new(
        RTCRtpCodecCapability {
            mime_type: MIME_TYPE_H264.to_owned(),
            ..Default::default()
        },
        "video".to_owned(),
        "remotebridge".to_owned(),
    ));
    peer_connection
        .add_track(Arc::clone(&video_track) as Arc<dyn TrackLocal + Send + Sync>)
        .await?;

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
        .send(&SignalMessage::Offer { sdp: local_desc.sdp })
        .await?;

    println!("offer sent, waiting for viewer's answer...");
    let answer_sdp = match signaling.recv().await? {
        SignalMessage::Answer { sdp } => sdp,
        SignalMessage::Offer { .. } => return Err("expected an answer but got another offer".into()),
    };
    peer_connection
        .set_remote_description(RTCSessionDescription::answer(answer_sdp)?)
        .await?;

    Ok((peer_connection, video_track))
}
