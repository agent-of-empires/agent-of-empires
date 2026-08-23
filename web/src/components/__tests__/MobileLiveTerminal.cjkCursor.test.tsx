// @vitest-environment jsdom
//
// Row boxes the live cursor cell by walking segment text and comparing a
// UTF-16-index-based running column against `cursorCol`, which is a real
// terminal cell count from tmux (issue #2665). Every CJK/wide character
// contributes 2 cells but only 1 UTF-16 code unit, so the running column
// under-counts and the boxed cell drifts right of the actual cursor.

import { describe, expect, it } from "vitest";
import { cellWidth } from "../../lib/liveTermLines";
import { render } from "@testing-library/react";
import { Row } from "../MobileLiveTerminal";
import type { AnsiSegment } from "../../lib/ansi";

function seg(text: string): AnsiSegment {
  return { text, style: {} };
}

function cursorCell(container: HTMLElement) {
  return container.querySelector("[data-live-cursor]");
}

describe("Row cursor placement with CJK (wide) characters", () => {
  it("boxes the cell immediately after CJK text with no drift", () => {
    // 7 Korean chars (2 cells each) + 3 digits (1 cell each) = 17 cells.
    // The cursor sits right after the last typed character.
    const text = "한글정렬테스트123";
    const { container } = render(<Row segs={[seg(text)]} cursorCol={17} />);
    const cell = cursorCell(container);
    expect(cell).not.toBeNull();
    // Per-cell runs (#3342): each CJK glyph gets its own fixed box, the
    // ASCII digits flow together. No pad span of spaces may appear before
    // the cursor; drift shows up as exactly that.
    const spans = [...container.querySelectorAll("span")];
    expect(spans.map((s) => s.textContent)).toEqual([..."한글정렬테스트", "123", " "]);
    expect(cell!.previousSibling!.textContent).toBe("123");
    expect(cell!.textContent).toBe(" ");
    // Fixed boxes carry explicit widths; the cursor cell (a space) is one
    // of them. Each Hangul glyph is one code point in a two-cell box.
    const fixed = spans.filter((s) => s.style.display === "inline-block");
    expect(fixed).toHaveLength(8);
    for (const box of fixed) {
      expect(box.style.width).toBe(`calc(var(--term-cell, 1em) * ${cellWidth([...box.textContent!][0]!)})`);
    }
  });

  it("boxes the correct character in a mixed ASCII+CJK line", () => {
    // "hello " is 6 cells; then CJK chars are 2 cells each: 한(6-8) 글(8-10)
    // 정(10-12) 렬(12-14). Column 8 must land on "글", not "정".
    const segs = [seg("hello "), seg("한글정렬")];
    const { container } = render(<Row segs={segs} cursorCol={8} />);
    const cell = cursorCell(container);
    expect(cell).not.toBeNull();
    expect(cell!.textContent).toBe("글"); // 글
  });
});
