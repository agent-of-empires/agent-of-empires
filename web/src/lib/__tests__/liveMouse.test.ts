import { describe, expect, it } from "vitest";
import { buttonMouseBytes, wheelMouseBytes, wheelNotches } from "../liveMouse";

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
