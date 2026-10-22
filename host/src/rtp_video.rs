//! Sends encoded H.264 frames as RTP packets stamped with when each frame
//! was captured.
//!
//! webrtc-rs's TrackLocalStaticSample can't do this: it sets each frame's
//! RTP timestamp to the sum of the *previous* samples' durations, so a
//! frame's real gap is only known one frame too late. ScreenCaptureKit
//! delivers frames only when the screen changes, so the gaps vary from 33 ms
//! to seconds. With timestamps that don't follow capture time, the browser
//! sees frames arriving "late" by varying amounts, takes that as network
//! jitter and holds more video before showing it: measured, its jitter
//! buffer grew past one second. Here the timestamp is the capture time, so
//! a still screen just looks like a long gap between frames.

use std::sync::Arc;
use std::time::Instant;

use bytes::Bytes;
use webrtc::rtp::codecs::h264::H264Payloader;
use webrtc::rtp::header::Header;
use webrtc::rtp::packet::Packet;
use webrtc::rtp::packetizer::Payloader;
use webrtc::track::track_local::TrackLocalWriter;
use webrtc::track::track_local::track_local_static_rtp::TrackLocalStaticRTP;

/// H.264 over RTP always uses a 90 kHz clock (RFC 6184).
const CLOCK_RATE: f64 = 90_000.0;

/// RTP payload size per packet, kept under a typical path MTU once IP, UDP,
/// SRTP and TURN headers are added. webrtc-rs uses the same 1200.
const RTP_PAYLOAD_SIZE: usize = 1200;

/// The RTP timestamp for a frame captured at `captured`. RTP timestamps
/// are 32 bits and wrap (after ~13 hours at 90 kHz); receivers expect that.
fn rtp_timestamp(start: Instant, captured: Instant) -> u32 {
    let ticks = captured.saturating_duration_since(start).as_secs_f64() * CLOCK_RATE;
    (ticks as u64 & 0xffff_ffff) as u32
}

pub struct VideoSender {
    track: Arc<TrackLocalStaticRTP>,
    payloader: H264Payloader,
    sequence: u16,
    start: Instant,
}

impl VideoSender {
    pub fn new(track: Arc<TrackLocalStaticRTP>) -> Self {
        Self {
            track,
            payloader: H264Payloader::default(),
            sequence: 0,
            start: Instant::now(),
        }
    }

    /// Splits one encoded frame (Annex B) into packets and writes them. The
    /// track fills in SSRC and payload type for each connected viewer.
    /// Before any viewer, the track has no bindings and this sends nothing.
    pub async fn write_frame(
        &mut self,
        frame: Bytes,
        captured: Instant,
    ) -> webrtc::error::Result<()> {
        let timestamp = rtp_timestamp(self.start, captured);
        let payloads = self.payloader.payload(RTP_PAYLOAD_SIZE, &frame)?;
        let count = payloads.len();
        for (i, payload) in payloads.into_iter().enumerate() {
            let packet = Packet {
                header: Header {
                    version: 2,
                    // The marker bit flags a frame's last packet, telling the
                    // receiver the frame is complete and can be decoded.
                    marker: i == count - 1,
                    sequence_number: self.sequence,
                    timestamp,
                    ..Default::default()
                },
                payload,
            };
            self.sequence = self.sequence.wrapping_add(1);
            self.track.write_rtp(&packet).await?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn timestamp_follows_capture_time_at_90khz() {
        let start = Instant::now();
        assert_eq!(rtp_timestamp(start, start), 0);
        assert_eq!(
            rtp_timestamp(start, start + Duration::from_millis(500)),
            45_000
        );
        // A frame captured before start (it can't be, but never panic).
        assert_eq!(rtp_timestamp(start + Duration::from_secs(1), start), 0);
    }

    #[test]
    fn timestamp_wraps_at_32_bits() {
        let start = Instant::now();
        let wrap = Duration::from_secs_f64(f64::from(u32::MAX) / CLOCK_RATE + 1.0);
        assert!(rtp_timestamp(start, start + wrap) < 100_000);
    }
}
