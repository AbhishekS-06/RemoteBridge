// Tests for client/input.mjs. Run from the repository root:
//   node --test tests/client/*.test.mjs
import { test } from "node:test";
import assert from "node:assert/strict";
import {
  frameRect, framePosition, createMouse, createTrackpad,
  keyMessage, typingMessage, SENTINEL, TAP_MAX_MS, DRAG_AFTER_TAP_MS,
} from "../../client/input.mjs";

// --- frame mapping ---

test("frameRect letterboxes a 16:9 frame in a tall element", () => {
  // 1280x720 drawn in a 390x844 phone screen: full width, centred vertically.
  const f = frameRect(1280, 720, { left: 0, top: 0, width: 390, height: 844 });
  assert.equal(f.width, 390);
  assert.ok(Math.abs(f.height - 219.375) < 1e-9);
  assert.ok(Math.abs(f.top - (844 - 219.375) / 2) < 1e-9);
  assert.equal(f.left, 0);
});

test("frameRect pillarboxes in a wide element", () => {
  const f = frameRect(1280, 720, { left: 10, top: 0, width: 2000, height: 720 });
  assert.equal(f.height, 720);
  assert.equal(f.width, 1280);
  assert.equal(f.left, 10 + 360);
});

test("frameRect is null before the video has a size", () => {
  assert.equal(frameRect(0, 0, { left: 0, top: 0, width: 100, height: 100 }), null);
});

test("framePosition gives 0..1 fractions inside the frame", () => {
  const f = { left: 100, top: 50, width: 200, height: 100 };
  assert.deepEqual(framePosition(f, 200, 100), { x: 0.5, y: 0.5 });
  assert.deepEqual(framePosition(f, 100, 50), { x: 0, y: 0 });
  assert.equal(framePosition(null, 1, 1), null);
});

// --- mouse ---

function recorder() {
  const sent = [];
  return { sent, send: (m) => sent.push(m) };
}

test("mouse press, drag and release send down/move/up", () => {
  const r = recorder();
  const m = createMouse(r.send);
  assert.equal(m.handle({ type: "pointerdown", button: 0 }, { x: 0.2, y: 0.3 }), true);
  m.handle({ type: "pointermove" }, { x: 1.5, y: -0.2 });
  m.handle({ type: "pointerup" }, { x: 0.4, y: 0.5 });
  assert.deepEqual(r.sent, [
    { type: "down", x: 0.2, y: 0.3 },
    { type: "move", x: 1, y: 0 }, // clamped while dragging outside the frame
    { type: "up", x: 0.4, y: 0.5 },
  ]);
});

test("mouse ignores presses on the black bars and other buttons", () => {
  const r = recorder();
  const m = createMouse(r.send);
  assert.equal(m.handle({ type: "pointerdown", button: 0 }, { x: 0.5, y: -0.1 }), false);
  assert.equal(m.handle({ type: "pointerdown", button: 2 }, { x: 0.5, y: 0.5 }), false);
  m.handle({ type: "pointermove" }, { x: 0.5, y: 0.5 });
  m.handle({ type: "pointerup" }, { x: 0.5, y: 0.5 });
  assert.deepEqual(r.sent, []);
});

// --- trackpad ---

// A 200x100 frame makes the expected fractions easy to read.
function trackpad() {
  const r = recorder();
  const t = createTrackpad(r.send, () => ({ width: 200, height: 100 }));
  return { sent: r.sent, t };
}
const ev = (pointerId, clientX, clientY, timeStamp, type = "pointerdown") =>
  ({ pointerId, clientX, clientY, timeStamp, type });

test("one-finger tap is a left click", () => {
  const { sent, t } = trackpad();
  t.down(ev(1, 50, 50, 1000));
  t.move(ev(1, 53, 52, 1050, "pointermove")); // wobble inside the slop
  t.up(ev(1, 53, 52, 1100, "pointerup"));
  assert.deepEqual(sent, [{ type: "click", button: "left" }]);
});

test("a held touch is not a tap", () => {
  const { sent, t } = trackpad();
  t.down(ev(1, 50, 50, 1000));
  t.up(ev(1, 50, 50, 1000 + TAP_MAX_MS + 1, "pointerup"));
  assert.deepEqual(sent, []);
});

test("one finger sliding moves the cursor by frame fractions, no click", () => {
  const { sent, t } = trackpad();
  t.down(ev(1, 50, 50, 1000));
  t.move(ev(1, 70, 50, 1020, "pointermove")); // 20 px: past the slop
  t.move(ev(1, 90, 60, 1040, "pointermove"));
  t.up(ev(1, 90, 60, 1060, "pointerup"));
  assert.deepEqual(sent, [
    { type: "move_rel", dx: 0.1, dy: 0 },
    { type: "move_rel", dx: 0.1, dy: 0.1 },
  ]);
});

test("tap then touch-and-slide drags with the button held", () => {
  const { sent, t } = trackpad();
  t.down(ev(1, 50, 50, 1000));
  t.up(ev(1, 50, 50, 1050, "pointerup"));
  t.down(ev(2, 50, 50, 1050 + DRAG_AFTER_TAP_MS - 1));
  t.move(ev(2, 70, 50, 1400, "pointermove"));
  t.up(ev(2, 70, 50, 1450, "pointerup"));
  assert.deepEqual(sent, [
    { type: "click", button: "left" },
    { type: "press", button: "left" },
    { type: "move_rel", dx: 0.1, dy: 0 },
    { type: "release", button: "left" },
  ]);
});

test("touching again too late after a tap moves instead of dragging", () => {
  const { sent, t } = trackpad();
  t.down(ev(1, 50, 50, 1000));
  t.up(ev(1, 50, 50, 1050, "pointerup"));
  t.down(ev(2, 50, 50, 1050 + DRAG_AFTER_TAP_MS));
  t.move(ev(2, 70, 50, 1400, "pointermove"));
  assert.deepEqual(sent.slice(1), [{ type: "move_rel", dx: 0.1, dy: 0 }]);
});

test("two-finger tap is a right click", () => {
  const { sent, t } = trackpad();
  t.down(ev(1, 50, 50, 1000));
  t.down(ev(2, 80, 50, 1010));
  t.up(ev(1, 50, 50, 1080, "pointerup"));
  t.up(ev(2, 80, 50, 1090, "pointerup"));
  assert.deepEqual(sent, [{ type: "click", button: "right" }]);
});

test("two fingers sliding scroll, and lifting one does not move the cursor", () => {
  const { sent, t } = trackpad();
  t.down(ev(1, 50, 50, 1000));
  t.down(ev(2, 70, 50, 1000));
  t.move(ev(1, 50, 70, 1020, "pointermove")); // centre moves 10 px
  t.move(ev(2, 70, 70, 1030, "pointermove")); // and 10 px more
  t.up(ev(1, 50, 70, 1040, "pointerup"));
  t.move(ev(2, 70, 90, 1050, "pointermove")); // one finger left, still a scroll
  t.up(ev(2, 70, 90, 1060, "pointerup"));
  assert.deepEqual(sent, [
    { type: "scroll", dx: 0, dy: 0.1 },
    { type: "scroll", dx: 0, dy: 0.1 },
    { type: "scroll", dx: 0, dy: 0.2 }, // the remaining finger moved 20 px
  ]);
});

test("a second finger during a drag releases the button", () => {
  const { sent, t } = trackpad();
  t.down(ev(1, 50, 50, 1000));
  t.up(ev(1, 50, 50, 1050, "pointerup"));
  t.down(ev(2, 50, 50, 1100));
  t.move(ev(2, 70, 50, 1150, "pointermove"));
  t.down(ev(3, 100, 50, 1200));
  assert.deepEqual(sent.slice(-1), [{ type: "release", button: "left" }]);
});

test("pointercancel never clicks", () => {
  const { sent, t } = trackpad();
  t.down(ev(1, 50, 50, 1000));
  t.up(ev(1, 50, 50, 1050, "pointercancel"));
  assert.deepEqual(sent, []);
});

test("nothing moves before the video has a size", () => {
  const sent = [];
  const t = createTrackpad((m) => sent.push(m), () => null);
  t.down(ev(1, 50, 50, 1000));
  t.move(ev(1, 90, 50, 1020, "pointermove"));
  assert.deepEqual(sent, []);
});

// --- keyboard ---

const key = (k, extra = {}) => ({ key: k, code: "", ...extra });

test("characters are text, unless the phone field is focused", () => {
  assert.deepEqual(keyMessage(key("a"), false), { type: "text", text: "a" });
  // U+1D11E is two UTF-16 units but one character.
  assert.deepEqual(keyMessage(key("\u{1D11E}"), false), { type: "text", text: "\u{1D11E}" });
  assert.equal(keyMessage(key("a"), true), null);
});

test("named keys carry their modifiers", () => {
  assert.deepEqual(keyMessage(key("Enter"), false), { type: "key", key: "Enter", mods: [] });
  assert.deepEqual(keyMessage(key("ArrowLeft", { shiftKey: true, altKey: true }), false),
    { type: "key", key: "ArrowLeft", mods: ["shift", "alt"] });
});

test("backspace in the phone field comes from the field, not keydown", () => {
  assert.equal(keyMessage(key("Backspace"), true), null);
  assert.deepEqual(keyMessage(key("Backspace"), false), { type: "key", key: "Backspace", mods: [] });
});

test("shortcuts use the physical key, not the shifted character", () => {
  assert.deepEqual(keyMessage(key("$", { code: "Digit4", metaKey: true, shiftKey: true }), true),
    { type: "key", key: "4", mods: ["shift", "meta"] });
  assert.deepEqual(keyMessage(key("c", { code: "KeyC", metaKey: true }), false),
    { type: "key", key: "c", mods: ["meta"] });
  assert.deepEqual(keyMessage(key("Tab", { code: "Tab", metaKey: true }), false),
    { type: "key", key: "Tab", mods: ["meta"] });
});

test("modifiers alone, F-keys and IME composition send nothing", () => {
  assert.equal(keyMessage(key("Shift", { shiftKey: true }), false), null);
  assert.equal(keyMessage(key("F5"), false), null);
  assert.equal(keyMessage(key("F5", { metaKey: true }), false), null);
  assert.equal(keyMessage(key("a", { isComposing: true }), false), null);
});

// --- phone keyboard field ---

test("typed characters after the sentinel are sent as text", () => {
  assert.deepEqual(typingMessage(SENTINEL + "h"), { type: "text", text: "h" });
  assert.deepEqual(typingMessage(SENTINEL + "hello"), { type: "text", text: "hello" });
});

test("an untouched field sends nothing", () => {
  assert.equal(typingMessage(SENTINEL), null);
});

test("deleting the sentinel is a backspace", () => {
  assert.deepEqual(typingMessage(""), { type: "key", key: "Backspace", mods: [] });
});

test("typing over the sentinel sends the text without it", () => {
  assert.deepEqual(typingMessage("ok"), { type: "text", text: "ok" });
  assert.deepEqual(typingMessage("o" + SENTINEL + "k"), { type: "text", text: "ok" });
});
