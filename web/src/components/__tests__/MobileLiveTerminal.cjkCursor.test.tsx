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
    // The CJK stretch coalesces into ONE fixed box (#3342 follow-up):
    // atomic-inline boundaries inside the stretch would break bidi,
    // complex-script shaping and emoji composition. The box pins the
    // flow/fixed boundary at exactly 14 cells; no pad span of spaces may
    // appear before the cursor.
    const spans = [...container.querySelectorAll("span")];
    expect(spans.map((s) => s.textContent)).toEqual(["한글정렬테스트", "123", " "]);
    expect(spans[0].style.width).toBe("calc(var(--term-cell, 1em) * 14)");
    expect(cell!.previousSibling!.textContent).toBe("123");
    expect(cell!.textContent).toBe(" ");
    expect(cell!.style.width).toBe("calc(var(--term-cell, 1em) * 1)");
  });

  it("slices a coalesced stretch at cluster boundaries under the cursor", () => {
    // Cursor on the second glyph of a CJK word: pre and cursor pieces are
    // separately boxed at their exact cell widths, nothing drifts.
    const { container } = render(<Row segs={[seg("한글")]} cursorCol={2} />);
    const spans = [...container.querySelectorAll("span")];
    expect(spans.map((s) => s.textContent)).toEqual(["한", "글"]);
    expect(spans[0].style.width).toBe("calc(var(--term-cell, 1em) * 2)");
    expect(cursorCell(container)!.textContent).toBe("글");
  });

  it("keeps trailing combining marks inside the boxed cursor cell", () => {
    // clusterSpanAt must take the WHOLE cluster (base plus marks): slicing
    // the base alone would strand its marks in a zero-width sibling and
    // browsers draw dotted circles.
    const text = "\u6F22\u0301"; // CJK base + combining acute
    const { container } = render(<Row segs={[seg(text)]} cursorCol={0} />);
    const cell = cursorCell(container);
    expect(cell!.textContent).toBe(text);
    expect(cell!.style.width).toBe(`calc(var(--term-cell, 1em) * ${cellWidth("\u6F22")})`);
    // No sibling spans: the whole row lives in the cursor cell.
    expect(container.querySelectorAll("span")).toHaveLength(1);
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
