use std::fs::File;
use std::sync::mpsc::{self, Sender};

use ffmpeg_next as ffmpeg;
use screencapturekit::cm::CMSampleBufferExt;
use screencapturekit::cv::CVPixelBufferLockFlags;
use screencapturekit::prelude::*;

mod encoder;

const CAPTURE_WIDTH: u32 = 1280;
const CAPTURE_HEIGHT: u32 = 720;

// Frame rate and length for the synthetic "gradient" test path. This has
// nothing to do with the real capture rate above; it just needs to be long
// enough to cross a few GOP boundaries (one keyframe per second, per
// encoder.rs) and produce visible motion.
const GRADIENT_FPS: i32 = 30;
const GRADIENT_SECONDS: u32 = 5;

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

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = std::env::args().collect();
    if args.get(1).map(String::as_str) == Some("gradient") {
        return run_gradient();
    }

    let content = SCShareableContent::get()?;
    let display = &content.displays()[0];

    let filter = SCContentFilter::create()
        .with_display(display)
        .with_excluding_windows(&[])
        .build();

    let config = SCStreamConfiguration::new()
        .with_width(CAPTURE_WIDTH)
        .with_height(CAPTURE_HEIGHT)
        .with_pixel_format(PixelFormat::BGRA);

    let (tx, rx) = mpsc::channel();

    let mut stream = SCStream::new(&filter, &config);
    stream.add_output_handler(Handler { tx }, SCStreamOutputType::Screen);
    stream.start_capture()?;

    // Block until the capture thread hands us the first frame.
    let frame = rx.recv()?;
    stream.stop_capture()?;

    let image = image::RgbaImage::from_raw(frame.width, frame.height, frame.rgba)
        .ok_or("frame buffer size did not match dimensions")?;
    image.save("frame.png")?;
    println!("saved frame.png ({}x{})", frame.width, frame.height);
    Ok(())
}
