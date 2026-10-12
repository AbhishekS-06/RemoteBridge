//! Turns pointer events from the viewer into mouse events on this Mac.
//!
//! The viewer sends positions as fractions (0..1) of the video frame, so it
//! never needs to know the Mac's screen size, and the host converts them to
//! screen points here. The conversion has to undo the capture's letterbox:
//! ScreenCaptureKit scales the display into the 1280x720 frame keeping its
//! aspect ratio, so a 16:10-ish MacBook screen sits between black bars on
//! the left and right of the frame.
//!
//! Injection runs on one dedicated thread that owns the `Enigo` handle. The
//! data channel callbacks only parse and forward events, so they never block
//! on the OS, and every session shares the same injector.

use std::sync::mpsc;

use enigo::{Button, Coordinate, Direction, Enigo, Mouse, Settings};
use serde::Deserialize;

/// One pointer event from the viewer, as JSON on the input data channel:
/// `{"type":"down","x":0.5,"y":0.25}`. Only the primary touch is sent: down
/// presses the left button, move drags (or just moves), up releases.
#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum InputEvent {
    Down { x: f64, y: f64 },
    Move { x: f64, y: f64 },
    Up { x: f64, y: f64 },
    /// Sent by the host itself, never by the viewer, when an input channel
    /// closes. A viewer that disconnects mid-drag never sends its "up", and
    /// without this the left button would stay held down on the Mac.
    #[serde(skip)]
    Reset,
}

pub type InputSender = mpsc::Sender<InputEvent>;

/// Starts the injector thread and returns the sender sessions use to feed
/// it. If the thread cannot start injecting (no Accessibility permission),
/// it prints why and exits; sends then fail and are ignored, and streaming
/// carries on without input.
pub fn start_injector(frame_width: u32, frame_height: u32) -> InputSender {
    let (tx, rx) = mpsc::channel::<InputEvent>();
    std::thread::spawn(move || {
        // On macOS this checks the Accessibility permission and, if it is
        // missing, opens the System Settings prompt. Like Screen Recording,
        // it is granted to the terminal app the host runs from.
        let mut enigo = match Enigo::new(&Settings::default()) {
            Ok(enigo) => enigo,
            Err(e) => {
                println!(
                    "input disabled: {e} (allow the terminal in System Settings > \
                     Privacy & Security > Accessibility, then restart the host)"
                );
                return;
            }
        };
        let display = match enigo.main_display() {
            Ok(size) => size,
            Err(e) => {
                println!("input disabled: cannot read display size: {e}");
                return;
            }
        };
        let map = FrameMap::new(frame_width, frame_height, display.0, display.1);
        println!("input ready, display {}x{} points", display.0, display.1);

        let mut pressed = false;
        for event in rx {
            if let Err(e) = apply(&mut enigo, &map, &mut pressed, event) {
                println!("input injection failed: {e}");
            }
        }
    });
    tx
}

fn apply(
    enigo: &mut Enigo,
    map: &FrameMap,
    pressed: &mut bool,
    event: InputEvent,
) -> Result<(), enigo::InputError> {
    match event {
        // enigo presses the button wherever the cursor currently is, so the
        // cursor is moved to the touch point first.
        InputEvent::Down { x, y } => {
            let (px, py) = map.to_screen(x, y);
            enigo.move_mouse(px, py, Coordinate::Abs)?;
            enigo.button(Button::Left, Direction::Press)?;
            *pressed = true;
        }
        // With the button held, enigo sends a drag event instead of a plain
        // move, which is what lets a swipe drag a window or select text.
        InputEvent::Move { x, y } => {
            let (px, py) = map.to_screen(x, y);
            enigo.move_mouse(px, py, Coordinate::Abs)?;
        }
        InputEvent::Up { x, y } => {
            let (px, py) = map.to_screen(x, y);
            enigo.move_mouse(px, py, Coordinate::Abs)?;
            enigo.button(Button::Left, Direction::Release)?;
            *pressed = false;
        }
        InputEvent::Reset => {
            if *pressed {
                enigo.button(Button::Left, Direction::Release)?;
                *pressed = false;
            }
        }
    }
    Ok(())
}

/// Where the display sits inside the captured frame. ScreenCaptureKit
/// scales the display by the largest factor that still fits the frame and
/// centres it, so the bars are equal on both sides.
struct FrameMap {
    frame_width: f64,
    frame_height: f64,
    display_width: f64,
    display_height: f64,
    /// Frame pixels per display point.
    scale: f64,
    /// Width of one side bar, and height of one top/bottom bar, in frame
    /// pixels. One of the two is always zero.
    offset_x: f64,
    offset_y: f64,
}

impl FrameMap {
    fn new(frame_width: u32, frame_height: u32, display_width: i32, display_height: i32) -> Self {
        let (fw, fh) = (frame_width as f64, frame_height as f64);
        let (dw, dh) = (display_width as f64, display_height as f64);
        let scale = (fw / dw).min(fh / dh);
        FrameMap {
            frame_width: fw,
            frame_height: fh,
            display_width: dw,
            display_height: dh,
            scale,
            offset_x: (fw - dw * scale) / 2.0,
            offset_y: (fh - dh * scale) / 2.0,
        }
    }

    /// Frame fractions to display points. A touch on a black bar is clamped
    /// to the nearest screen edge rather than dropped, so a drag that slides
    /// off the edge still ends where the finger left the screen.
    fn to_screen(&self, x: f64, y: f64) -> (i32, i32) {
        let px = (x * self.frame_width - self.offset_x) / self.scale;
        let py = (y * self.frame_height - self.offset_y) / self.scale;
        (
            px.clamp(0.0, self.display_width - 1.0).round() as i32,
            py.clamp(0.0, self.display_height - 1.0).round() as i32,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // A 14" MacBook Pro at its default scaling (1512x982 points) captured
    // into 1280x720: scale 720/982, content 1108.6 px wide, bars 85.7 px.
    fn macbook() -> FrameMap {
        FrameMap::new(1280, 720, 1512, 982)
    }

    #[test]
    fn centre_maps_to_centre() {
        assert_eq!(macbook().to_screen(0.5, 0.5), (756, 491));
    }

    #[test]
    fn content_edges_map_to_screen_edges() {
        let map = macbook();
        let left = map.offset_x / 1280.0;
        let right = 1.0 - left;
        assert_eq!(map.to_screen(left, 0.0), (0, 0));
        assert_eq!(map.to_screen(right, 1.0), (1511, 981));
    }

    #[test]
    fn bars_clamp_to_nearest_edge() {
        let map = macbook();
        assert_eq!(map.to_screen(0.01, 0.5).0, 0);
        assert_eq!(map.to_screen(0.99, 0.5).0, 1511);
    }

    #[test]
    fn parses_viewer_json() {
        let event: InputEvent = serde_json::from_str(r#"{"type":"down","x":0.25,"y":1}"#).unwrap();
        assert!(matches!(event, InputEvent::Down { x, y } if x == 0.25 && y == 1.0));
        assert!(serde_json::from_str::<InputEvent>(r#"{"type":"reset"}"#).is_err());
    }
}
