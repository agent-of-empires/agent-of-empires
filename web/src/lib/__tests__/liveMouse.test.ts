import { describe, expect, it } from "vitest";
import { cursorLineIndex, pointerPaneCell, buttonMouseBytes, wheelMouseBytes, wheelNotches } from "../liveMouse";

const bytes = (...n: number[]) => new Uint8Array(n);
const ascii = (s: string) => new Uint8Array([...s].map((c) => c.charCodeAt(0)));

describe("wheelMouseBytes", () => {
  it("encodes SGR wheel up/down at a 1-based cell", () => {
    expect(wheelMouseBytes(true, true, 3, 3)).toEqual(ascii("\x1b[<64;3;3M"));
    expect(wheelMouseBytes(false, true, 3, 3)).toEqual(ascii("\x1b[<65;3;3M"));
  });

  it("encodes legacy X10 wheel up/down (value + 32, ESC [ M prefix)", () => {
    // wheel up = button 64 -> 0x60; col/row 3 -> 0x23.
    expect(wheelMouseBytes(true, false, 3, 3)).toEqual(bytes(0x1b, 0x5b, 0x4d, 64 + 32, 3 + 32, 3 + 32));
    expect(wheelMouseBytes(false, false, 3, 3)).toEqual(bytes(0x1b, 0x5b, 0x4d, 65 + 32, 3 + 32, 3 + 32));
  });

  it("clamps legacy coordinates at 223 (single-byte limit)", () => {
    expect(wheelMouseBytes(true, false, 300, 300)).toEqual(bytes(0x1b, 0x5b, 0x4d, 64 + 32, 223 + 32, 223 + 32));
  });

  it("floors coordinates to at least 1", () => {
    expect(wheelMouseBytes(true, true, 0, -5)).toEqual(ascii("\x1b[<64;1;1M"));
  });
});

describe("buttonMouseBytes", () => {
  it("encodes SGR press/release for left/middle/right (M vs m)", () => {
    expect(buttonMouseBytes(0, false, false, true, 5, 7)).toEqual(ascii("\x1b[<0;5;7M"));
    expect(buttonMouseBytes(1, false, false, true, 5, 7)).toEqual(ascii("\x1b[<1;5;7M"));
    expect(buttonMouseBytes(2, false, false, true, 5, 7)).toEqual(ascii("\x1b[<2;5;7M"));
    // Release keeps button identity but ends with lowercase m.
    expect(buttonMouseBytes(0, true, false, true, 5, 7)).toEqual(ascii("\x1b[<0;5;7m"));
  });

  it("sets the SGR drag (motion) bit at +32", () => {
    expect(buttonMouseBytes(0, false, true, true, 5, 7)).toEqual(ascii("\x1b[<32;5;7M"));
    expect(buttonMouseBytes(2, false, true, true, 5, 7)).toEqual(ascii("\x1b[<34;5;7M"));
  });

  it("encodes legacy X10 press with the motion bit and value + 32", () => {
    expect(buttonMouseBytes(0, false, false, false, 3, 3)).toEqual(bytes(0x1b, 0x5b, 0x4d, 0 + 32, 3 + 32, 3 + 32));
    expect(buttonMouseBytes(0, false, true, false, 3, 3)).toEqual(bytes(0x1b, 0x5b, 0x4d, 32 + 32, 3 + 32, 3 + 32));
  });

  it("uses the agnostic button 3 for a legacy X10 release", () => {
    expect(buttonMouseBytes(2, true, false, false, 3, 3)).toEqual(bytes(0x1b, 0x5b, 0x4d, 3 + 32, 3 + 32, 3 + 32));
  });

  it("clamps legacy coordinates at 223 and floors cells to 1", () => {
    expect(buttonMouseBytes(0, false, false, false, 300, 300)).toEqual(
      bytes(0x1b, 0x5b, 0x4d, 0 + 32, 223 + 32, 223 + 32),
    );
    expect(buttonMouseBytes(0, false, false, true, 0, -5)).toEqual(ascii("\x1b[<0;1;1M"));
  });
});

describe("wheelNotches", () => {
  it("converts accumulated pixels into whole notches and keeps the remainder", () => {
    expect(wheelNotches(50, 16, 8)).toEqual({ notches: 3, remainder: 2 });
    expect(wheelNotches(-50, 16, 8)).toEqual({ notches: -3, remainder: -2 });
  });

  it("caps notches per event so a flick can't flood", () => {
    expect(wheelNotches(1000, 16, 8)).toEqual({ notches: 8, remainder: 1000 - 8 * 16 });
  });

  it("emits nothing below one notch, carrying the sub-notch remainder", () => {
    expect(wheelNotches(10, 16, 8)).toEqual({ notches: 0, remainder: 10 });
  });

  it("is a no-op for a zero threshold", () => {
    expect(wheelNotches(99, 0, 8)).toEqual({ notches: 0, remainder: 99 });
  });
});

describe("cursorLineIndex", () => {
  it("indexes the live edge when every line fits on screen", () => {
    // 72-line composite fully visible: cursor row indexes directly.
    expect(cursorLineIndex(72, 72, 63)).toBe(63);
  });

  it("keeps the mapping when scrolled back (screenRows < lines.length)", () => {
    // The viewport shows the LAST screenRows lines of the capture.
    expect(cursorLineIndex(120, 72, 0)).toBe(48);
    expect(cursorLineIndex(120, 72, 63)).toBe(111);
  });

  it("clamps the live edge at zero when the viewport is taller than the capture", () => {
    // A short capture in a tall viewport must index from the top, not go
    // negative (which the renderer reads as "no cursor").
    expect(cursorLineIndex(5, 72, 3)).toBe(3);
  });
});

describe("pointerPaneCell", () => {
  it("is the identity when pane 0 sits at the window origin", () => {
    expect(pointerPaneCell(10, 5, { cols: 80, rows: 24 })).toEqual({ col: 10, row: 6 });
    expect(pointerPaneCell(10, 5, { cols: 80, rows: 24, left: 0, top: 0 })).toEqual({
      col: 10,
      row: 6,
    });
  });

  it("subtracts a non-zero origin (#3515: pane-border-status shifts pane 0 down)", () => {
    // Composite cell (10, 5) over a pane starting one row down: the app
    // hears its own row 5, not the border row above it.
    expect(pointerPaneCell(10, 5, { cols: 164, rows: 71, left: 0, top: 1 })).toEqual({
      col: 10,
      row: 5,
    });
    // A right-hand neighbour's columns (pane 0 spans composite 1-based
    // cols 166..329 here) clamp into pane 0; the web surface clamps rather
    // than drops forwarded events.
    expect(pointerPaneCell(340, 5, { cols: 164, rows: 71, left: 165, top: 1 })).toEqual({
      col: 164,
      row: 5,
    });
    // Inside the neighbour-adjacent pane itself the origin subtraction lands
    // on the app's own column.
    expect(pointerPaneCell(170, 5, { cols: 164, rows: 71, left: 165, top: 1 })).toEqual({
      col: 5,
      row: 5,
    });
  });

  it("clamps to pane 0's rectangle", () => {
    expect(pointerPaneCell(500, 100, { cols: 164, rows: 71, left: 0, top: 1 })).toEqual({
      col: 164,
      row: 71,
    });
    expect(pointerPaneCell(0, 0, { cols: 164, rows: 71, left: 0, top: 1 })).toEqual({
      col: 1,
      row: 1,
    });
  });

  it("treats a missing rectangle as a 1x1 pane", () => {
    expect(pointerPaneCell(7, 9, null)).toEqual({ col: 1, row: 1 });
  });
});
