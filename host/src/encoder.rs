//! H.264 encoding via libx264 through FFmpeg.
//!
//! The job of this module: take raw YUV420P frames in, produce compressed
//! H.264 bytes out. It knows nothing about screen capture or files. That
//! separation is deliberate: milestone 3 feeds it captured frames instead of
//! synthetic ones, and v1 sends its output to WebRTC instead of a file, and
//! neither change touches this module.

use std::error::Error;
use std::io::Write;

// The crate is named ffmpeg-next on crates.io, but its Rust name is
// ffmpeg_next. Aliasing to `ffmpeg` keeps the rest of the file readable.
use ffmpeg_next as ffmpeg;
use ffmpeg::{Dictionary, Packet, Rational, codec, encoder, format, frame};

pub struct H264Encoder {
    // The opened encoder. FFmpeg calls this a "codec context": it holds the
    // configuration (size, format, settings) plus the encoder's internal
    // state, including its memory of previous frames used for prediction.
    encoder: encoder::Video,

    // A packet is one chunk of compressed output. We keep one and reuse it
    // for every receive_packet call rather than allocating a new one per
    // frame. FFmpeg refills it each time.
    packet: Packet,
}

impl H264Encoder {
    /// Create an encoder for `width` x `height` frames at `fps`.
    pub fn new(width: u32, height: u32, fps: i32) -> Result<Self, Box<dyn Error>> {
        // Registers codecs and sets up FFmpeg's global state. Safe to call
        // more than once; only the first call does anything.
        ffmpeg::init()?;

        // Ask FFmpeg for the libx264 encoder by name. If your FFmpeg was built
        // without it, this returns None and we bail with a clear message.
        let codec = encoder::find_by_name("libx264")
            .ok_or("libx264 not found in this FFmpeg build")?;

        // Build an unconfigured context for that codec, then narrow it to a
        // video encoder. The .encoder().video() chain is the crate's way of
        // giving us the type with video-specific setters like set_width.
        let mut ctx = codec::context::Context::new_with_codec(codec)
            .encoder()
            .video()?;

        ctx.set_width(width);
        ctx.set_height(height);

        // libx264 requires planar YUV with 4:2:0 chroma subsampling. Whatever
        // format our frames start in, they must be converted to this first.
        ctx.set_format(format::Pixel::YUV420P);

        // The time base is the unit for timestamps. Setting it to 1/fps means
        // a frame's pts (presentation timestamp) is just its frame number:
        // pts 0 is the first frame, pts 60 is one second in at 60 fps.
        ctx.set_time_base(Rational(1, fps));
        ctx.set_frame_rate(Some(Rational(fps, 1)));

        // GOP (group of pictures) is the keyframe interval. A keyframe is a
        // full self-contained image; the frames between keyframes only store
        // differences. One keyframe per second means a viewer who joins or
        // loses a packet is back to a clean picture within a second.
        ctx.set_gop(fps as u32);

        // B-frames predict from both the previous AND the next frame. They
        // compress better but the encoder must wait for the next frame before
        // it can emit them, which adds delay. For remote desktop, latency
        // matters more than a few percent of bandwidth, so turn them off.
        ctx.set_max_b_frames(0);

        // Options that libx264 understands but the generic FFmpeg API has no
        // setter for. These go in as string key/value pairs.
        let mut opts = Dictionary::new();

        // preset: how much CPU to spend searching for good predictions.
        // ultrafast .. veryslow. veryfast is a good real-time starting point.
        opts.set("preset", "veryfast");

        // tune=zerolatency disables the encoder's lookahead buffer, so a frame
        // goes in and its packet comes out immediately instead of several
        // frames later.
        opts.set("tune", "zerolatency");

        // crf: constant rate factor, a quality target. Lower is better quality
        // and bigger files. 23 is the libx264 default. In v2 this will likely
        // become a bitrate cap instead, driven by measured bandwidth.
        opts.set("crf", "23");

        // open_with consumes the unconfigured context and returns the opened
        // encoder. After this point the settings are locked in.
        let encoder = ctx.open_with(opts)?;

        Ok(Self {
            encoder,
            packet: Packet::empty(),
        })
    }

    /// Encode one frame. Any packets the encoder is ready to emit are written to `out`.
    ///
    /// The encoder is a pipeline, not a function: sending one frame does not
    /// guarantee exactly one packet comes out. With zerolatency it usually
    /// does, but the API contract is "send, then drain whatever is ready".
    pub fn encode<W: Write>(&mut self, frame: &frame::Video, out: &mut W) -> Result<(), Box<dyn Error>> {
        self.encoder.send_frame(frame)?;
        self.drain(out)
    }

    /// Tell the encoder there are no more frames and flush what it still holds.
    ///
    /// Takes `self` by value, so the encoder is destroyed after this call and
    /// the compiler stops you from accidentally encoding more afterward.
    pub fn finish<W: Write>(mut self, out: &mut W) -> Result<(), Box<dyn Error>> {
        self.encoder.send_eof()?;
        self.drain(out)
    }

    /// Pull every packet the encoder currently has ready and write it out.
    fn drain<W: Write>(&mut self, out: &mut W) -> Result<(), Box<dyn Error>> {
        // receive_packet returns Ok while packets are available. It returns
        // Err(EAGAIN) when the encoder wants more input before it can produce
        // another, or Err(EOF) after finish() once everything is flushed.
        // Both mean "stop pulling for now", so we don't treat them as failures.
        while self.encoder.receive_packet(&mut self.packet).is_ok() {
            if let Some(data) = self.packet.data() {
                // With libx264 and no GLOBAL_HEADER flag, each packet is
                // already in Annex B format: NAL units separated by start
                // codes, with the SPS/PPS headers included before keyframes.
                // Concatenating packets therefore gives a playable .h264 file.
                out.write_all(data)?;
            }
        }
        Ok(())
    }
}
