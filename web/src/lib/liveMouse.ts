// Mouse-wheel forwarding for the mobile live view. When the live-send
// target is a full-screen (alternate-screen) app with mouse tracking on,
// its scrollback is not capturable, so scroll gestures forward the wheel
// to the app as input bytes instead of widening the capture window. This
// mirrors the TUI's `wheel_mouse_bytes` (src/tui/home/input.rs) so both
// surfaces speak the same encodings. See src/server/live_ws.rs for the
// frame flags (altScreen / mouse / mouseSgr) that drive this.

/**
 * Build the wheel byte sequence for a full-screen mouse app. `up` selects
 * wheel-up (button 64) vs wheel-down (65); `sgr` picks SGR (1006) vs the
 * legacy X10 encoding, matching what the app enabled. `col`/`row` are
 * 1-based pane cells.
 */
export function wheelMouseBytes(up: boolean, sgr: boolean, col: number, row: number): Uint8Array<ArrayBuffer> {
  const button = up ? 64 : 65;
  const cx = Math.max(1, Math.floor(col));
  const cy = Math.max(1, Math.floor(row));
  // Both branches build via `new Uint8Array(len)` (a fresh, non-shared
  // ArrayBuffer) so the result is exactly what WebSocket.send accepts.
  if (sgr) {
    // SGR (1006): textual, press marker `M`. Pure ASCII, no coord limit.
    const s = `\x1b[<${button};${cx};${cy}M`;
    const out = new Uint8Array(s.length);
    for (let i = 0; i < s.length; i++) out[i] = s.charCodeAt(i);
    return out;
  }
  // Legacy X10: `ESC [ M` then three bytes, each value + 32. Bytes top out
  // at 255, so coordinates above 223 can't be encoded; clamp there.
  const enc = (v: number) => Math.min(223, v) + 32;
  const out = new Uint8Array(6);
  out.set([0x1b, 0x5b, 0x4d, enc(button), enc(cx), enc(cy)]);
  return out;
}

/**
 * Build a mouse button report (press / drag / release) for a full-screen
 * mouse app, mirroring the TUI's `mouse_event_bytes` (src/tui/home/input.rs)
 * so both surfaces speak the same encodings. `baseButton` is 0/1/2 for
 * left/middle/right; `motion` sets the drag bit (button held while moving);
 * `release` marks a button-up. `sgr` picks SGR (1006) vs legacy X10. `col`/
 * `row` are 1-based pane cells.
 */
export function buttonMouseBytes(
  baseButton: number,
  release: boolean,
  motion: boolean,
  sgr: boolean,
  col: number,
  row: number,
): Uint8Array<ArrayBuffer> {
  // The drag (motion) bit rides on press/drag reports in both encodings.
  const cb = baseButton + (motion ? 32 : 0);
  const cx = Math.max(1, Math.floor(col));
  const cy = Math.max(1, Math.floor(row));
  if (sgr) {
    // SGR (1006): press/drag end with `M`, release with `m`, so the button
    // identity is preserved on release. Pure ASCII, no coord limit.
    const end = release ? "m" : "M";
    const s = `\x1b[<${cb};${cx};${cy}${end}`;
    const out = new Uint8Array(s.length);
    for (let i = 0; i < s.length; i++) out[i] = s.charCodeAt(i);
    return out;
  }
  // Legacy X10: `ESC [ M` then three bytes, each value + 32 (clamped at 223).
  // A release can't carry button identity, so it uses the agnostic button 3.
  const enc = (v: number) => Math.min(223, v) + 32;
  const btn = release ? 3 : cb;
  const out = new Uint8Array(6);
  out.set([0x1b, 0x5b, 0x4d, enc(btn), enc(cx), enc(cy)]);
  return out;
}

/**
 * Convert an accumulated scroll delta (in pixels, positive = scroll toward
 * newer/down) into a whole number of wheel notches plus the leftover that
 * didn't reach a full notch. `thresholdPx` is the pixels per notch (one
 * text row). The leftover is fed back in on the next event so a slow drag
 * still scrolls smoothly without losing motion. `maxNotches` caps a single
 * event so a fast flick can't flood the agent.
 */
export function wheelNotches(
  accumPx: number,
  thresholdPx: number,
  maxNotches: number,
): { notches: number; remainder: number } {
  if (thresholdPx <= 0) return { notches: 0, remainder: accumPx };
  const raw = Math.trunc(accumPx / thresholdPx);
  const notches = Math.max(-maxNotches, Math.min(maxNotches, raw));
  return { notches, remainder: accumPx - notches * thresholdPx };
}
