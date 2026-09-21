// Mouse forwarding for full-screen apps in the mobile live view, mirroring the TUI's encodings in src/tui/home/input.rs.

/** Wheel bytes: `up` picks button 64 vs 65, `sgr` picks SGR (1006) vs X10; `col`/`row` are 1-based. */
export function wheelMouseBytes(up: boolean, sgr: boolean, col: number, row: number): Uint8Array<ArrayBuffer> {
  const button = up ? 64 : 65;
  const cx = Math.max(1, Math.floor(col));
  const cy = Math.max(1, Math.floor(row));
  // A fresh non-shared ArrayBuffer, which is what WebSocket.send accepts.
  if (sgr) {
    const s = `\x1b[<${button};${cx};${cy}M`;
    const out = new Uint8Array(s.length);
    for (let i = 0; i < s.length; i++) out[i] = s.charCodeAt(i);
    return out;
  }
  // Legacy X10 encodes value + 32 in one byte, so coordinates clamp at 223.
  const enc = (v: number) => Math.min(223, v) + 32;
  const out = new Uint8Array(6);
  out.set([0x1b, 0x5b, 0x4d, enc(button), enc(cx), enc(cy)]);
  return out;
}

/** Button report mirroring the TUI's `mouse_event_bytes`. `baseButton` is 0/1/2; `motion` sets the drag bit. */
export function buttonMouseBytes(
  baseButton: number,
  release: boolean,
  motion: boolean,
  sgr: boolean,
  col: number,
  row: number,
): Uint8Array<ArrayBuffer> {
  const cb = baseButton + (motion ? 32 : 0);
  const cx = Math.max(1, Math.floor(col));
  const cy = Math.max(1, Math.floor(row));
  if (sgr) {
    // SGR ends a release with `m`, preserving the button identity.
    const end = release ? "m" : "M";
    const s = `\x1b[<${cb};${cx};${cy}${end}`;
    const out = new Uint8Array(s.length);
    for (let i = 0; i < s.length; i++) out[i] = s.charCodeAt(i);
    return out;
  }
  // X10 cannot carry the button on release, so it uses button 3.
  const enc = (v: number) => Math.min(223, v) + 32;
  const btn = release ? 3 : cb;
  const out = new Uint8Array(6);
  out.set([0x1b, 0x5b, 0x4d, enc(btn), enc(cx), enc(cy)]);
  return out;
}

/** Whole wheel notches from a pixel delta, returning the leftover for the next event. `maxNotches` caps a fast flick. */
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

export function cursorLineIndex(lineCount: number, screenRows: number, cursorY: number): number {
  return Math.max(0, lineCount - screenRows) + cursorY;
}

/** Map window-grid coordinates to pane 0's 1-based mouse cell, clamped to the pane. */
export function pointerPaneCell(
  compositeCol: number,
  compositeRow: number,
  pane0: { cols: number; rows: number; left?: number; top?: number } | null | undefined,
): { col: number; row: number } {
  const left = pane0?.left ?? 0;
  const top = pane0?.top ?? 0;
  const cols = pane0?.cols ?? 1;
  const rows = pane0?.rows ?? 1;
  return {
    col: Math.min(cols, Math.max(1, compositeCol - left)),
    row: Math.min(rows, Math.max(1, compositeRow - top + 1)),
  };
}
