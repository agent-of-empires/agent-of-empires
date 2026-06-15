import { describe, expect, it } from "vitest";
import { wheelMouseBytes, wheelNotches } from "../liveMouse";

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
