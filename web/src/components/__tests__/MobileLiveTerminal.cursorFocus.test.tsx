// @vitest-environment jsdom
//
// The cursor cell always rendered as a hollow outline, with no path for the
// input's focus state to reach it (#2684). `Row` now takes a `focused` prop
// and picks a filled style (solid background, inverted text) when true,
// keeping the hollow outline for the default/blurred case so existing
// placement tests (MobileLiveTerminal.cjkCursor.test.tsx) need no changes.
// Focused also gets the blink animation class; jsdom doesn't run the CSS
// animation itself, but the class presence is what drives it in a browser.

import { describe, expect, it } from "vitest";
import { render } from "@testing-library/react";
import { Row } from "../MobileLiveTerminal";
import type { AnsiSegment } from "../../lib/ansi";

function seg(text: string): AnsiSegment {
  return { text, style: {} };
}

function cursorCell(container: HTMLElement) {
  return container.querySelector("[data-live-cursor]") as HTMLElement | null;
}

describe("Row cursor fill state", () => {
  it("renders a hollow outline, no fill, no blink, when unfocused (default)", () => {
    const { container } = render(<Row segs={[seg("hi")]} cursorCol={2} />);
    const cell = cursorCell(container);
    expect(cell).not.toBeNull();
    expect(cell!.style.outline).toContain("var(--term-cursor");
    expect(cell!.style.backgroundColor).toBe("");
    expect(cell!.className).toBe("");
  });

  it("renders a hollow outline, no fill, no blink, when focused=false", () => {
    const { container } = render(<Row segs={[seg("hi")]} cursorCol={2} focused={false} />);
    const cell = cursorCell(container);
    expect(cell!.style.outline).toContain("var(--term-cursor");
    expect(cell!.style.backgroundColor).toBe("");
    expect(cell!.className).toBe("");
  });

  it("fills solid with inverted text color and blinks when focused=true", () => {
    const { container } = render(<Row segs={[seg("hi")]} cursorCol={2} focused />);
    const cell = cursorCell(container);
    expect(cell).not.toBeNull();
    expect(cell!.style.backgroundColor).toContain("var(--term-cursor");
    expect(cell!.style.color).toContain("var(--term-bg");
    expect(cell!.style.outline).toBe("");
    expect(cell!.className).toContain("animate-term-cursor-blink");
  });

  it("fills and blinks the blank-cell cursor (cursor past row text) when focused", () => {
    const { container } = render(<Row segs={[seg("hi")]} cursorCol={5} focused />);
    const cell = cursorCell(container);
    expect(cell).not.toBeNull();
    expect(cell!.style.backgroundColor).toContain("var(--term-cursor");
    expect(cell!.style.outline).toBe("");
    expect(cell!.className).toContain("animate-term-cursor-blink");
  });
});
