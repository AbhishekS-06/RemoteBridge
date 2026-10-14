//! Turns pointer and keyboard events from the viewer into input on this Mac.
//!
//! The viewer sends positions and distances as fractions (0..1) of the video
//! frame, so it never needs to know the Mac's screen size, and the host
//! converts them to screen points here. The conversion has to undo the
//! capture's letterbox: ScreenCaptureKit scales the display into the
//! 1280x720 frame keeping its aspect ratio, so a 16:10-ish MacBook screen
//! sits between black bars on the left and right of the frame.
//!
//! Two kinds of event arrive. A mouse on a desktop browser points directly
//! (down/move/up at a position). A phone's touchscreen acts as a trackpad:
//! the page recognises the gestures and sends relative moves, clicks,
//! button presses for drags, and scrolls.
//!
//! Keyboard input comes in two kinds as well. Typed characters arrive as
//! text and are entered as text, so the Mac never has to work out which
//! keys and which layout would produce them. Keys that are not characters
//! (enter, arrows, escape) and shortcuts (Cmd+C) arrive as a key plus the
//! modifiers held with it, and are pressed as keys.
//!
//! Injection runs on one dedicated thread that owns the `Enigo` handle. The
//! data channel callbacks only parse and forward events, so they never block
//! on the OS, and every session shares the same injector.

use std::sync::mpsc;

use enigo::{Axis, Button, Coordinate, Direction, Enigo, Key, Keyboard, Mouse, Settings};
use serde::Deserialize;

/// One input event from the viewer, as JSON on the input data channel,
/// for example `{"type":"down","x":0.5,"y":0.25}`,
/// `{"type":"click","button":"right"}` or
/// `{"type":"key","key":"c","mods":["meta"]}`.
#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum InputEvent {
    /// Direct pointing (desktop mouse): down presses the left button at the
    /// position, move drags (or just moves), up releases.
    Down {
        x: f64,
        y: f64,
    },
    Move {
        x: f64,
        y: f64,
    },
    Up {
        x: f64,
        y: f64,
    },
    /// Trackpad (touch): move the cursor by a distance, in frame fractions.
    MoveRel {
        dx: f64,
        dy: f64,
    },
    /// Press and release at the cursor (a tap, or a two-finger tap).
    Click {
        button: MouseButton,
    },
    /// Hold and release a button at the cursor (tap then drag).
    Press {
        button: MouseButton,
    },
    Release {
        button: MouseButton,
    },
    /// Scroll by a distance, in frame fractions, in the direction the
    /// fingers moved (content follows the fingers, as on a phone).
    Scroll {
        dx: f64,
        dy: f64,
    },
    /// Characters the viewer typed, entered as they are.
    Text {
        text: String,
    },
    /// One key pressed and released with the modifiers held around it.
    /// The whole combination is one message, so no modifier is ever left
    /// held on the Mac between messages.
    Key {
        key: KeyName,
        #[serde(default)]
        mods: Vec<Modifier>,
    },
    /// Sent by the host itself, never by the viewer, when an input channel
    /// closes. A viewer that disconnects mid-drag never sends its "up", and
    /// without this the button would stay held down on the Mac.
    #[serde(skip)]
    Reset,
}

#[derive(Debug, Clone, Copy, PartialEq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum MouseButton {
    Left,
    Right,
}

impl MouseButton {
    fn to_enigo(self) -> Button {
        match self {
            MouseButton::Left => Button::Left,
            MouseButton::Right => Button::Right,
        }
    }
}

/// A key, named as the browser names it (`KeyboardEvent.key`), so the page
/// forwards the name unchanged. Only the keys below are accepted, and a
/// message with any other name is rejected when it is parsed. A single
/// character (`"c"`) is the key that types it, for shortcuts.
#[derive(Debug, Clone, Copy, PartialEq, Deserialize)]
#[serde(try_from = "String")]
pub struct KeyName(Key);

impl TryFrom<String> for KeyName {
    type Error = String;

    fn try_from(name: String) -> Result<Self, Self::Error> {
        let key = match name.as_str() {
            "Enter" => Key::Return,
            "Backspace" => Key::Backspace,
            // Forward delete, which macOS laptops type as fn+delete.
            "Delete" => Key::Delete,
            "Tab" => Key::Tab,
            "Escape" => Key::Escape,
            " " => Key::Space,
            "ArrowUp" => Key::UpArrow,
            "ArrowDown" => Key::DownArrow,
            "ArrowLeft" => Key::LeftArrow,
            "ArrowRight" => Key::RightArrow,
            "Home" => Key::Home,
            "End" => Key::End,
            "PageUp" => Key::PageUp,
            "PageDown" => Key::PageDown,
            _ => {
                let mut chars = name.chars();
                match (chars.next(), chars.next()) {
                    (Some(c), None) if c != '\0' => Key::Unicode(c),
                    _ => return Err(format!("unknown key {name:?}")),
                }
            }
        };
        Ok(KeyName(key))
    }
}

/// The modifiers the viewer can hold with a key, named as the browser's
/// `KeyboardEvent` flags name them (`metaKey` is Command on a Mac).
#[derive(Debug, Clone, Copy, PartialEq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Modifier {
    Shift,
    Control,
    Alt,
    Meta,
}

impl Modifier {
    fn to_enigo(self) -> Key {
        match self {
            Modifier::Shift => Key::Shift,
            Modifier::Control => Key::Control,
            Modifier::Alt => Key::Option,
            Modifier::Meta => Key::Meta,
        }
    }
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
        let enigo = match Enigo::new(&Settings::default()) {
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

        let mut injector = Injector {
            enigo,
            map,
            held: Vec::new(),
            move_rest: (0.0, 0.0),
            scroll_rest: (0.0, 0.0),
        };
        for event in rx {
            if let Err(e) = injector.apply(event) {
                println!("input injection failed: {e}");
            }
        }
    });
    tx
}

/// The injector thread's state: which buttons it is holding down, so they
/// can be released on Reset, and the fractions of a point left over from
/// earlier relative moves and scrolls.
struct Injector {
    enigo: Enigo,
    map: FrameMap,
    held: Vec<MouseButton>,
    /// enigo moves and scrolls in whole points, and a slow finger moves less
    /// than one point per event. Dropping the remainder each time would
    /// make slow movement stall, so it carries over to the next event.
    move_rest: (f64, f64),
    scroll_rest: (f64, f64),
}

impl Injector {
    fn apply(&mut self, event: InputEvent) -> Result<(), enigo::InputError> {
        match event {
            // enigo presses the button wherever the cursor currently is, so
            // the cursor is moved to the touch point first.
            InputEvent::Down { x, y } => {
                let (px, py) = self.map.to_screen(x, y);
                self.enigo.move_mouse(px, py, Coordinate::Abs)?;
                self.press(MouseButton::Left)?;
            }
            // With the button held, enigo sends a drag event instead of a
            // plain move, which is what lets a swipe drag a window or select
            // text. The same holds for MoveRel during a tap-then-drag.
            InputEvent::Move { x, y } => {
                let (px, py) = self.map.to_screen(x, y);
                self.enigo.move_mouse(px, py, Coordinate::Abs)?;
            }
            InputEvent::Up { x, y } => {
                let (px, py) = self.map.to_screen(x, y);
                self.enigo.move_mouse(px, py, Coordinate::Abs)?;
                self.release(MouseButton::Left)?;
            }
            // The new position is computed here and sent as absolute,
            // rather than using enigo's Coordinate::Rel, because enigo does
            // not keep a relative move on the screen (a TODO in its macOS
            // code); this clamps it to the display.
            InputEvent::MoveRel { dx, dy } => {
                let (dx, dy) = self.map.to_points(dx, dy);
                let step_x = take_whole(&mut self.move_rest.0, dx);
                let step_y = take_whole(&mut self.move_rest.1, dy);
                if step_x != 0 || step_y != 0 {
                    let (cx, cy) = self.enigo.location()?;
                    let x = (cx + step_x).clamp(0, self.map.display_width as i32 - 1);
                    let y = (cy + step_y).clamp(0, self.map.display_height as i32 - 1);
                    self.enigo.move_mouse(x, y, Coordinate::Abs)?;
                }
            }
            // enigo counts quick repeated clicks itself and marks them as
            // double (and triple) clicks, so two taps open a file.
            InputEvent::Click { button } => {
                self.enigo.button(button.to_enigo(), Direction::Click)?;
            }
            InputEvent::Press { button } => self.press(button)?,
            InputEvent::Release { button } => self.release(button)?,
            // smooth_scroll takes pixels; positive scrolls down or right,
            // which moves the content up or left. The fingers moving up
            // (negative dy) should move the content up, hence the minus.
            InputEvent::Scroll { dx, dy } => {
                let (dx, dy) = self.map.to_points(dx, dy);
                let step_x = take_whole(&mut self.scroll_rest.0, -dx);
                let step_y = take_whole(&mut self.scroll_rest.1, -dy);
                if step_y != 0 {
                    self.enigo.smooth_scroll(step_y, Axis::Vertical)?;
                }
                if step_x != 0 {
                    self.enigo.smooth_scroll(step_x, Axis::Horizontal)?;
                }
            }
            // enigo posts text as one keyboard event carrying the string,
            // with every modifier flag cleared, so it types exactly these
            // characters whatever the Mac's keyboard layout is.
            InputEvent::Text { text } => self.enigo.text(&text)?,
            InputEvent::Key { key, mods } => self.key_combo(key, &mods)?,
            InputEvent::Reset => {
                for button in std::mem::take(&mut self.held) {
                    self.enigo.button(button.to_enigo(), Direction::Release)?;
                }
            }
        }
        Ok(())
    }

    fn press(&mut self, button: MouseButton) -> Result<(), enigo::InputError> {
        self.enigo.button(button.to_enigo(), Direction::Press)?;
        if !self.held.contains(&button) {
            self.held.push(button);
        }
        Ok(())
    }

    fn release(&mut self, button: MouseButton) -> Result<(), enigo::InputError> {
        self.enigo.button(button.to_enigo(), Direction::Release)?;
        self.held.retain(|&b| b != button);
        Ok(())
    }

    /// Presses the modifiers, clicks the key, then releases the modifiers
    /// in reverse order. enigo remembers which modifiers are down and sets
    /// them as flags on the key's event, which is what makes Cmd+C a copy.
    /// Every modifier that went down is released even if a later step
    /// fails, because a Command key stuck down on the Mac would turn every
    /// local keypress into a shortcut.
    fn key_combo(&mut self, key: KeyName, mods: &[Modifier]) -> Result<(), enigo::InputError> {
        let mut pressed = Vec::new();
        let mut result = Ok(());
        for &m in mods {
            result = self.enigo.key(m.to_enigo(), Direction::Press);
            if result.is_err() {
                break;
            }
            pressed.push(m);
        }
        if result.is_ok() {
            result = self.enigo.key(key.0, Direction::Click);
        }
        for m in pressed.into_iter().rev() {
            let released = self.enigo.key(m.to_enigo(), Direction::Release);
            result = result.and(released);
        }
        result
    }
}

/// Adds `delta` to the carried-over remainder and takes out the whole
/// points, leaving the fraction for next time.
fn take_whole(rest: &mut f64, delta: f64) -> i32 {
    *rest += delta;
    let whole = rest.trunc();
    *rest -= whole;
    whole as i32
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

    /// A distance in frame fractions to a distance in display points. Unlike
    /// a position it has no offset: the bars only shift where the screen
    /// starts, not how far a movement goes.
    fn to_points(&self, dx: f64, dy: f64) -> (f64, f64) {
        (
            dx * self.frame_width / self.scale,
            dy * self.frame_height / self.scale,
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
    fn full_frame_height_moves_full_screen_height() {
        // The display fills the frame vertically, so a move of the whole
        // frame height is the whole screen height. Horizontally the frame
        // is wider than the screen content, so a full-width move goes past
        // the screen's width (by the two bars).
        let (dx, dy) = macbook().to_points(1.0, 1.0);
        assert!((dy - 982.0).abs() < 1e-9);
        assert!((dx - 1280.0 * 982.0 / 720.0).abs() < 1e-9);
    }

    #[test]
    fn remainders_carry_over() {
        // Four moves of 0.3 points: nothing, nothing, nothing, then one
        // point (1.2 total), with 0.2 left.
        let mut rest = 0.0;
        let steps: Vec<i32> = (0..4).map(|_| take_whole(&mut rest, 0.3)).collect();
        assert_eq!(steps, vec![0, 0, 0, 1]);
        assert!((rest - 0.2).abs() < 1e-9);
        // Negative movement truncates towards zero the same way.
        let mut rest = 0.0;
        assert_eq!(take_whole(&mut rest, -2.7), -2);
        assert!((rest + 0.7).abs() < 1e-9);
    }

    #[test]
    fn parses_viewer_json() {
        let event: InputEvent = serde_json::from_str(r#"{"type":"down","x":0.25,"y":1}"#).unwrap();
        assert!(matches!(event, InputEvent::Down { x, y } if x == 0.25 && y == 1.0));
        let event: InputEvent =
            serde_json::from_str(r#"{"type":"move_rel","dx":0.01,"dy":-0.02}"#).unwrap();
        assert!(matches!(event, InputEvent::MoveRel { dx, dy } if dx == 0.01 && dy == -0.02));
        let event: InputEvent =
            serde_json::from_str(r#"{"type":"click","button":"right"}"#).unwrap();
        assert!(matches!(
            event,
            InputEvent::Click {
                button: MouseButton::Right
            }
        ));
        let event: InputEvent =
            serde_json::from_str(r#"{"type":"scroll","dx":0,"dy":0.1}"#).unwrap();
        assert!(matches!(event, InputEvent::Scroll { .. }));
        assert!(serde_json::from_str::<InputEvent>(r#"{"type":"reset"}"#).is_err());
        assert!(
            serde_json::from_str::<InputEvent>(r#"{"type":"click","button":"middle"}"#).is_err()
        );
    }

    #[test]
    fn parses_keyboard_json() {
        let event: InputEvent = serde_json::from_str(r#"{"type":"text","text":"Hi!"}"#).unwrap();
        assert!(matches!(event, InputEvent::Text { text } if text == "Hi!"));
        // A named key without modifiers: mods defaults to empty.
        let event: InputEvent = serde_json::from_str(r#"{"type":"key","key":"Enter"}"#).unwrap();
        assert!(
            matches!(event, InputEvent::Key { key, mods } if key == KeyName(Key::Return) && mods.is_empty())
        );
        // A shortcut: one character plus modifiers.
        let event: InputEvent =
            serde_json::from_str(r#"{"type":"key","key":"z","mods":["meta","shift"]}"#).unwrap();
        assert!(matches!(event, InputEvent::Key { key, mods }
            if key == KeyName(Key::Unicode('z')) && mods == vec![Modifier::Meta, Modifier::Shift]));
    }

    #[test]
    fn rejects_unknown_keys() {
        for json in [
            r#"{"type":"key","key":"F13"}"#,
            r#"{"type":"key","key":"ab"}"#,
            r#"{"type":"key","key":""}"#,
            r#"{"type":"key","key":"a","mods":["fn"]}"#,
        ] {
            assert!(serde_json::from_str::<InputEvent>(json).is_err(), "{json}");
        }
    }
}
