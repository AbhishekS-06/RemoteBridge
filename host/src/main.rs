use std::sync::mpsc::{self, Sender};

use screencapturekit::cm::CMSampleBufferExt;
use screencapturekit::cv::CVPixelBufferLockFlags;
use screencapturekit::prelude::*;

const CAPTURE_WIDTH: u32 = 1280;
const CAPTURE_HEIGHT: u32 = 720;

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

fn main() -> Result<(), Box<dyn std::error::Error>> {
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
