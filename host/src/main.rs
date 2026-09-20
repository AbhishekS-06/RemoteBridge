use std::fs::File;
use std::sync::mpsc::{self, Sender};
use std::time::Duration;

use ffmpeg_next as ffmpeg;
use screencapturekit::cm::CMSampleBufferExt;
use screencapturekit::cv::CVPixelBufferLockFlags;
use screencapturekit::prelude::*;
use webrtc::media::Sample;

mod encoder;
mod signaling;
mod webrtc_host;

const CAPTURE_WIDTH: u32 = 1280;
const CAPTURE_HEIGHT: u32 = 720;

// Frame rate and length for the synthetic "gradient" test path. This has
// nothing to do with the real capture rate above; it just needs to be long
// enough to cross a few GOP boundaries (one keyframe per second, per
// encoder.rs) and produce visible motion.
const GRADIENT_FPS: i32 = 30;
const GRADIENT_SECONDS: u32 = 5;

// Frame rate and length for the real-capture-to-file test path. 30fps is
// plenty for screen content and keeps the encoder's workload modest.
const CAPTURE_FILE_FPS: u32 = 30;
const CAPTURE_FILE_SECONDS: u32 = 10;

/// One captured frame, already converted to tightly packed RGBA.
struct Frame {
    width: u32,
    height: u32,
    rgba: Vec<u8>,
}

/// Receives frames from ScreenCaptureKit on Apple's capture thread and
/// forwards them to the main thread over a channel.
struct Handler {
    tx: Sender<Frame>,
}

impl SCStreamOutputTrait for Handler {
    fn did_output_sample_buffer(&self, sample: CMSampleBuffer, _: SCStreamOutputType) {
        let Some(pixel_buffer) = sample.pixel_buffer() else {
            return;
        };
        let Ok(guard) = pixel_buffer.lock(CVPixelBufferLockFlags::READ_ONLY) else {
            return;
        };
        // SAFETY: the buffer is locked read-only for the lifetime of `guard`,
        // so the slice stays valid until we drop it at the end of this method.
        let Some(bytes) = (unsafe { guard.as_slice() }) else {
            return;
        };
        let frame = bgra_to_rgba(bytes, guard.width(), guard.height(), guard.bytes_per_row());
        // If main has already stopped listening, dropping the frame is fine.
        let _ = self.tx.send(frame);
    }
}

/// Copy a padded BGRA buffer into a tight RGBA buffer.
/// `stride` is bytes per row in the source, which may exceed width * 4.
fn bgra_to_rgba(src: &[u8], width: usize, height: usize, stride: usize) -> Frame {
    let mut rgba = Vec::with_capacity(width * height * 4);
    for row in src.chunks(stride).take(height) {
        for px in row[..width * 4].chunks_exact(4) {
            rgba.extend_from_slice(&[px[2], px[1], px[0], px[3]]);
        }
    }
    Frame {
        width: width as u32,
        height: height as u32,
        rgba,
    }
}

/// Open a ScreenCaptureKit stream on the main display and start delivering
/// frames to `handler`. Shared by the single-frame PNG path and the
/// continuous capture-to-h264 path below -- they differ only in stream
/// config and handler type, not in how the stream itself gets opened.
fn start_capture_stream<H: SCStreamOutputTrait + 'static>(
    config: SCStreamConfiguration,
    handler: H,
) -> Result<SCStream, Box<dyn std::error::Error>> {
    let content = SCShareableContent::get()?;
    let display = &content.displays()[0];

    let filter = SCContentFilter::create()
        .with_display(display)
        .with_excluding_windows(&[])
        .build();

    let mut stream = SCStream::new(&filter, &config);
    stream.add_output_handler(handler, SCStreamOutputType::Screen);
    stream.start_capture()?;
    Ok(stream)
}

/// One captured frame, still in BGRA order but with row padding stripped
/// (tightly packed). Unlike `Frame`, no channel swap: BGRA is what
/// ScreenCaptureKit hands us and it's also a pixel format ffmpeg understands
/// directly, so the h264 path never needs to touch RGBA at all.
struct RawFrame {
    width: u32,
    height: u32,
    bgra: Vec<u8>,
}

/// Same role as `Handler`, but for the continuous capture-to-h264 path: packs
/// BGRA instead of converting to RGBA, since there's no PNG at the end.
struct CaptureHandler {
    tx: Sender<RawFrame>,
}

impl SCStreamOutputTrait for CaptureHandler {
    fn did_output_sample_buffer(&self, sample: CMSampleBuffer, _: SCStreamOutputType) {
        let Some(pixel_buffer) = sample.pixel_buffer() else {
            return;
        };
        let Ok(guard) = pixel_buffer.lock(CVPixelBufferLockFlags::READ_ONLY) else {
            return;
        };
        let Some(bytes) = (unsafe { guard.as_slice() }) else {
            return;
        };
        let frame = pack_bgra(bytes, guard.width(), guard.height(), guard.bytes_per_row());
        let _ = self.tx.send(frame);
    }
}

/// Strip row padding from a captured BGRA buffer, same padding-aware pattern
/// as `bgra_to_rgba`, just without the per-pixel channel swap.
fn pack_bgra(src: &[u8], width: usize, height: usize, stride: usize) -> RawFrame {
    let mut bgra = Vec::with_capacity(width * height * 4);
    for row in src.chunks(stride).take(height) {
        bgra.extend_from_slice(&row[..width * 4]);
    }
    RawFrame {
        width: width as u32,
        height: height as u32,
        bgra,
    }
}

/// Same as `CaptureHandler`, but hands frames to an async consumer over a
/// tokio channel instead of std::sync::mpsc -- the WebRTC path awaits this
/// channel from inside the tokio runtime, `CaptureHandler`'s consumer doesn't.
struct WebRtcCaptureHandler {
    tx: tokio::sync::mpsc::Sender<RawFrame>,
}

impl SCStreamOutputTrait for WebRtcCaptureHandler {
    fn did_output_sample_buffer(&self, sample: CMSampleBuffer, _: SCStreamOutputType) {
        let Some(pixel_buffer) = sample.pixel_buffer() else {
            return;
        };
        let Ok(guard) = pixel_buffer.lock(CVPixelBufferLockFlags::READ_ONLY) else {
            return;
        };
        let Some(bytes) = (unsafe { guard.as_slice() }) else {
            return;
        };
        let frame = pack_bgra(bytes, guard.width(), guard.height(), guard.bytes_per_row());
        // blocking_send is correct here: this callback runs on Apple's
        // capture thread, not inside the tokio runtime.
        let _ = self.tx.blocking_send(frame);
    }
}

/// Copy a tightly packed `RawFrame` into an ffmpeg BGRA frame, which has its
/// own (possibly different) row alignment -- same stride-safety concern as
/// `gradient_frame`'s plane writes, just for one packed plane instead of
/// three.
fn bgra_frame(raw: &RawFrame) -> ffmpeg::frame::Video {
    let mut frame = ffmpeg::frame::Video::new(ffmpeg::format::Pixel::BGRA, raw.width, raw.height);
    let stride = frame.stride(0);
    let width_bytes = raw.width as usize * 4;
    let data = frame.data_mut(0);
    for row in 0..raw.height as usize {
        let src = &raw.bgra[row * width_bytes..(row + 1) * width_bytes];
        data[row * stride..row * stride + width_bytes].copy_from_slice(src);
    }
    frame
}

/// Milestone 3: capture real frames, convert BGRA -> YUV420P with ffmpeg's
/// software scaler (swscale), encode, and write a fixed-length H.264 file.
/// Same `H264Encoder` as the gradient path in every setting -- only the
/// frame source changes.
fn run_capture() -> Result<(), Box<dyn std::error::Error>> {
    let config = SCStreamConfiguration::new()
        .with_width(CAPTURE_WIDTH)
        .with_height(CAPTURE_HEIGHT)
        .with_pixel_format(PixelFormat::BGRA)
        .with_fps(CAPTURE_FILE_FPS);

    let (tx, rx) = mpsc::channel();
    let stream = start_capture_stream(config, CaptureHandler { tx })?;

    let mut h264_encoder =
        encoder::H264Encoder::new(CAPTURE_WIDTH, CAPTURE_HEIGHT, CAPTURE_FILE_FPS as i32)?;

    // Converts each BGRA frame to the YUV420P format H264Encoder requires.
    // Same source and destination size here -- this scaler is doing a pure
    // format conversion, not a resize.
    let mut scaler = ffmpeg::software::scaling::Context::get(
        ffmpeg::format::Pixel::BGRA,
        CAPTURE_WIDTH,
        CAPTURE_HEIGHT,
        ffmpeg::format::Pixel::YUV420P,
        CAPTURE_WIDTH,
        CAPTURE_HEIGHT,
        ffmpeg::software::scaling::Flags::FAST_BILINEAR,
    )?;

    let mut out = File::create("capture.h264")?;

    let frame_count = CAPTURE_FILE_FPS * CAPTURE_FILE_SECONDS;
    for i in 0..frame_count {
        // Blocks until the capture thread hands us the next frame. This
        // assumes frames arrive roughly evenly spaced at CAPTURE_FILE_FPS --
        // with_fps() requests that pace but ScreenCaptureKit doesn't
        // guarantee it exactly. Good enough to prove the pipeline end to
        // end; pacing off real per-frame timestamps is future work.
        let raw = rx.recv()?;
        let src_frame = bgra_frame(&raw);

        let mut yuv_frame = ffmpeg::frame::Video::empty();
        scaler.run(&src_frame, &mut yuv_frame)?;
        yuv_frame.set_pts(Some(i64::from(i)));

        h264_encoder.encode(&yuv_frame, &mut out)?;
    }

    stream.stop_capture()?;
    h264_encoder.finish(&mut out)?;

    println!(
        "saved capture.h264 ({frame_count} frames, {CAPTURE_WIDTH}x{CAPTURE_HEIGHT} @ {CAPTURE_FILE_FPS}fps)"
    );
    Ok(())
}

/// v1: same pipeline as `run_capture`, but frames go to a WebRTC video
/// track over the signaling handshake instead of to a file, and the loop
/// runs until the process is killed instead of stopping at a fixed count.
async fn run_webrtc() -> Result<(), Box<dyn std::error::Error>> {
    let mut signaling =
        signaling::SignalingClient::connect("ws://localhost:8080/ws?role=host").await?;
    let (_peer_connection, video_track) = webrtc_host::connect_host(&mut signaling).await?;

    let config = SCStreamConfiguration::new()
        .with_width(CAPTURE_WIDTH)
        .with_height(CAPTURE_HEIGHT)
        .with_pixel_format(PixelFormat::BGRA)
        .with_fps(CAPTURE_FILE_FPS);

    let (tx, mut rx) = tokio::sync::mpsc::channel(4);
    let stream = start_capture_stream(config, WebRtcCaptureHandler { tx })?;

    let mut h264_encoder =
        encoder::H264Encoder::new(CAPTURE_WIDTH, CAPTURE_HEIGHT, CAPTURE_FILE_FPS as i32)?;
    let mut scaler = ffmpeg::software::scaling::Context::get(
        ffmpeg::format::Pixel::BGRA,
        CAPTURE_WIDTH,
        CAPTURE_HEIGHT,
        ffmpeg::format::Pixel::YUV420P,
        CAPTURE_WIDTH,
        CAPTURE_HEIGHT,
        ffmpeg::software::scaling::Flags::FAST_BILINEAR,
    )?;

    println!("streaming, ctrl-c to stop");
    let mut frame_num: i64 = 0;
    while let Some(raw) = rx.recv().await {
        let src_frame = bgra_frame(&raw);
        let mut yuv_frame = ffmpeg::frame::Video::empty();
        scaler.run(&src_frame, &mut yuv_frame)?;
        yuv_frame.set_pts(Some(frame_num));
        frame_num += 1;

        let mut packet = Vec::new();
        h264_encoder.encode(&yuv_frame, &mut packet)?;
        if !packet.is_empty() {
            video_track
                .write_sample(&Sample {
                    data: packet.into(),
                    duration: Duration::from_secs_f64(1.0 / CAPTURE_FILE_FPS as f64),
                    ..Default::default()
                })
                .await?;
        }
    }

    stream.stop_capture()?;
    Ok(())
}

/// Build one synthetic YUV420P test frame: a horizontal luma ramp that
/// shifts sideways with `frame_num`, so playback shows motion instead of a
/// static image. Chroma is held at 128 (neutral), so it renders grayscale.
///
/// This exists purely to exercise `H264Encoder` without needing real capture
/// data — milestone 3 is what swaps this out for actual captured frames.
fn gradient_frame(width: u32, height: u32, frame_num: u32) -> ffmpeg::frame::Video {
    let mut frame = ffmpeg::frame::Video::new(ffmpeg::format::Pixel::YUV420P, width, height);

    // Luma (Y) plane: full resolution, one byte per pixel.
    let y_stride = frame.stride(0);
    let y_offset = frame_num as usize;
    let y_plane = frame.data_mut(0);
    for row in 0..height as usize {
        for col in 0..width as usize {
            y_plane[row * y_stride + col] = ((col + y_offset) % 256) as u8;
        }
    }

    // Chroma (U, V) planes: 4:2:0 subsampling means each is half width and
    // half height of the luma plane. Held flat at 128 (no color).
    let chroma_width = (width / 2) as usize;
    let chroma_height = (height / 2) as usize;
    for plane in [1, 2] {
        let stride = frame.stride(plane);
        let data = frame.data_mut(plane);
        for row in 0..chroma_height {
            data[row * stride..row * stride + chroma_width].fill(128);
        }
    }

    frame.set_pts(Some(frame_num as i64));
    frame
}

/// Milestone 2: encode synthetic gradient frames straight to `out.h264`,
/// with no screen capture involved. Proves the encode path (frame in,
/// H.264 bytes out) works before wiring it to real capture in milestone 3.
fn run_gradient() -> Result<(), Box<dyn std::error::Error>> {
    let mut h264_encoder = encoder::H264Encoder::new(CAPTURE_WIDTH, CAPTURE_HEIGHT, GRADIENT_FPS)?;
    let mut out = File::create("out.h264")?;

    let frame_count = GRADIENT_FPS as u32 * GRADIENT_SECONDS;
    for i in 0..frame_count {
        let frame = gradient_frame(CAPTURE_WIDTH, CAPTURE_HEIGHT, i);
        h264_encoder.encode(&frame, &mut out)?;
    }
    h264_encoder.finish(&mut out)?;

    println!(
        "saved out.h264 ({frame_count} frames, {CAPTURE_WIDTH}x{CAPTURE_HEIGHT} @ {GRADIENT_FPS}fps)"
    );
    Ok(())
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = std::env::args().collect();
    match args.get(1).map(String::as_str) {
        Some("gradient") => return run_gradient(),
        Some("capture") => return run_capture(),
        Some("webrtc") => return run_webrtc().await,
        _ => {}
    }

    let config = SCStreamConfiguration::new()
        .with_width(CAPTURE_WIDTH)
        .with_height(CAPTURE_HEIGHT)
        .with_pixel_format(PixelFormat::BGRA);

    let (tx, rx) = mpsc::channel();
    let stream = start_capture_stream(config, Handler { tx })?;

    // Block until the capture thread hands us the first frame.
    let frame = rx.recv()?;
    stream.stop_capture()?;

    let image = image::RgbaImage::from_raw(frame.width, frame.height, frame.rgba)
        .ok_or("frame buffer size did not match dimensions")?;
    image.save("frame.png")?;
    println!("saved frame.png ({}x{})", frame.width, frame.height);
    Ok(())
}
