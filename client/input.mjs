// Input logic for the viewer page, kept free of the DOM so Node can test it
// (tests/client/input.test.mjs). index.html wires these to real events and
// sends what they return over the "input" data channel.
//
// Positions and distances go to the host as fractions (0..1) of the video
// frame, so the page never needs the Mac's screen size; the host maps them
// to screen points (host/src/input.rs).

// The page letterboxes the frame (object-fit: contain), so fractions are
// measured against the area the video is actually drawn in, not the whole
// element. rect is the element's bounding box.
export function frameRect(videoWidth, videoHeight, rect) {
  if (!videoWidth || !videoHeight) return null;
  const scale = Math.min(rect.width / videoWidth, rect.height / videoHeight);
  return {
    left: rect.left + (rect.width - videoWidth * scale) / 2,
    top: rect.top + (rect.height - videoHeight * scale) / 2,
    width: videoWidth * scale,
    height: videoHeight * scale,
  };
}

export function framePosition(frame, clientX, clientY) {
  if (!frame) return null;
  return {
    x: (clientX - frame.left) / frame.width,
    y: (clientY - frame.top) / frame.height,
  };
}

const clamp01 = (v) => Math.min(Math.max(v, 0), 1);
const positionMessage = (type, pos) => ({ type, x: clamp01(pos.x), y: clamp01(pos.y) });

// A mouse (a browser on a computer) points directly: the left button
// presses, drags and releases at the pointer's position. handle() takes the
// pointer event and its frame position, and returns true when a press
// started, so the page can capture the pointer and keep receiving moves and
// the final up even if it leaves the video element.
export function createMouse(send) {
  let down = false;
  function handle(event, pos) {
    if (event.type === "pointerdown") {
      if (event.button !== 0) return false;
      // A press on the black bars around the video is not on the screen.
      if (!pos || pos.x < 0 || pos.x > 1 || pos.y < 0 || pos.y > 1) return false;
      down = true;
      send(positionMessage("down", pos));
      return true;
    }
    if (!down) return false;
    if (event.type === "pointermove") {
      if (pos) send(positionMessage("move", pos));
      return false;
    }
    // pointerup or pointercancel: either way the button is released.
    down = false;
    if (pos) send(positionMessage("up", pos));
    return false;
  }
  return { handle };
}

// A touchscreen acts as a trackpad. Pointing directly with a finger at a
// phone-sized picture of the Mac's screen hides the target and lands a few
// points off; moving the cursor relatively does not, and the video shows
// the Mac's cursor, so the finger can be anywhere on the page.
//   one finger slides: the cursor moves the same distance
//   one-finger tap: left click (two quick taps: double-click)
//   tap, then touch again and slide: drag with the left button held
//   two-finger tap: right click
//   two fingers slide: scroll, the content following the fingers
export const TAP_SLOP_PX = 10;        // a finger moving less than this is still tapping
export const TAP_MAX_MS = 300;        // a touch held longer than this is not a tap
export const DRAG_AFTER_TAP_MS = 300; // touching again this soon after a tap can drag
const TRACKPAD_SPEED = 1;             // cursor distance per finger distance

// frameSize() returns the drawn frame ({width, height}) or null before the
// video has a size; finger distances are divided by it.
export function createTrackpad(send, frameSize) {
  const touches = new Map(); // pointerId -> latest {x, y} in page pixels
  let gesture = null;        // the current gesture, from first touch to last lift
  let lastTapAt = 0;

  // The fingers' average position. With two fingers this is what moves
  // when they slide together.
  function touchCentre() {
    let x = 0, y = 0;
    for (const t of touches.values()) { x += t.x; y += t.y; }
    return { x: x / touches.size, y: y / touches.size };
  }

  function down(event) {
    if (touches.size === 0) {
      gesture = {
        startedAt: event.timeStamp,
        fingers: 1,
        travelled: 0,
        moved: false,
        dragArmed: event.timeStamp - lastTapAt < DRAG_AFTER_TAP_MS,
        dragging: false,
        last: null,
      };
    }
    touches.set(event.pointerId, { x: event.clientX, y: event.clientY });
    // Once a second finger lands the gesture stays a two-finger one until
    // every finger has lifted, so lifting one finger first does not turn
    // the end of a scroll into a cursor move.
    if (touches.size >= 2 && gesture.fingers === 1) {
      gesture.fingers = 2;
      if (gesture.dragging) {
        gesture.dragging = false;
        send({ type: "release", button: "left" });
      }
    }
    // Measure from here, so a finger landing does not count as the
    // centre moving.
    gesture.last = touchCentre();
  }

  function move(event) {
    if (!touches.has(event.pointerId)) return;
    touches.set(event.pointerId, { x: event.clientX, y: event.clientY });
    const centre = touchCentre();
    const dx = centre.x - gesture.last.x, dy = centre.y - gesture.last.y;
    gesture.last = centre;
    gesture.travelled += Math.hypot(dx, dy);
    // A finger wobbles a little during a tap. Ignoring movement until it
    // passes the slop keeps a tap's click where the cursor was.
    if (!gesture.moved) {
      if (gesture.travelled < TAP_SLOP_PX) return;
      gesture.moved = true;
    }
    const frame = frameSize();
    if (!frame) return;
    const fx = dx / frame.width, fy = dy / frame.height;
    if (gesture.fingers === 2) {
      send({ type: "scroll", dx: fx, dy: fy });
      return;
    }
    if (gesture.dragArmed && !gesture.dragging) {
      gesture.dragging = true;
      send({ type: "press", button: "left" });
    }
    send({ type: "move_rel", dx: fx * TRACKPAD_SPEED, dy: fy * TRACKPAD_SPEED });
  }

  function up(event) {
    if (!touches.has(event.pointerId)) return;
    touches.delete(event.pointerId);
    if (touches.size > 0) {
      gesture.last = touchCentre();
      return;
    }
    const ended = gesture;
    gesture = null;
    if (ended.dragging) {
      send({ type: "release", button: "left" });
      return;
    }
    // pointercancel means the browser took the touch for itself (an
    // incoming call, a system gesture): never click for it.
    if (event.type === "pointercancel") return;
    if (ended.moved || event.timeStamp - ended.startedAt > TAP_MAX_MS) return;
    if (ended.fingers === 2) {
      send({ type: "click", button: "right" });
    } else {
      send({ type: "click", button: "left" });
      lastTapAt = event.timeStamp;
    }
  }

  return { down, move, up };
}

// Keyboard. Characters go to the host as text ({type: "text"}) and the Mac
// enters them as they are. Keys that are not characters, and any key
// pressed with Cmd or Ctrl, go as a key name plus the modifiers held
// ({type: "key"}); the host accepts only the names in host/src/input.rs
// (KeyName).
export const NAMED_KEYS = new Set([
  "Enter", "Backspace", "Delete", "Tab", "Escape",
  "ArrowUp", "ArrowDown", "ArrowLeft", "ArrowRight",
  "Home", "End", "PageUp", "PageDown",
]);

function modifiers(event) {
  const mods = [];
  if (event.shiftKey) mods.push("shift");
  if (event.ctrlKey) mods.push("control");
  if (event.altKey) mods.push("alt");
  if (event.metaKey) mods.push("meta");
  return mods;
}

// [...s] counts characters, not UTF-16 units, so a character outside the BMP is one.
const isCharacter = (key) => [...key].length === 1;

// The key a shortcut is on. With Shift held the browser reports the shifted
// character (Cmd+Shift+4 gives "$"), and the host would then look for a key
// that types "$". event.code names the physical key ("Digit4", "KeyZ"),
// which gives back the unshifted character.
function shortcutKey(event) {
  const m = /^(?:Key([A-Z])|Digit([0-9]))$/.exec(event.code);
  if (m) return (m[1] || m[2]).toLowerCase();
  return event.key;
}

// The message for a keydown, or null when there is nothing to send.
// inField is true while the hidden field has focus (the phone's keyboard is
// up): characters and backspace then come from the field's input events
// instead (typingMessage), because a phone keyboard does not reliably report
// them as keydowns, so here they return null.
export function keyMessage(event, inField) {
  if (event.isComposing) return null;
  if (event.metaKey || event.ctrlKey) {
    if (!isCharacter(event.key) && !NAMED_KEYS.has(event.key)) return null;
    return { type: "key", key: shortcutKey(event), mods: modifiers(event) };
  }
  if (NAMED_KEYS.has(event.key)) {
    if (inField && event.key === "Backspace") return null;
    return { type: "key", key: event.key, mods: modifiers(event) };
  }
  if (isCharacter(event.key)) {
    if (inField) return null;
    return { type: "text", text: event.key };
  }
  return null; // a bare modifier, a dead key, F-keys: nothing to send
}

// The phone's keyboard types into the hidden field. The field always holds
// one zero-width space with the caret after it, so backspace always has
// something to delete (on an empty field it fires no event). After each
// edit the field's value is compared with that, the difference sent, and
// the field put back.
export const SENTINEL = "​";

export function typingMessage(value) {
  if (value.startsWith(SENTINEL)) {
    return value.length > 1 ? { type: "text", text: value.slice(1) } : null;
  }
  if (value === "") return { type: "key", key: "Backspace", mods: [] };
  // Something replaced the sentinel itself (selected and typed over): send
  // what is there now, minus any sentinel left inside it.
  return { type: "text", text: value.replaceAll(SENTINEL, "") };
}
