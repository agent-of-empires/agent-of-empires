// @vitest-environment jsdom
// `cursorCol` counts terminal cells, and wide glyphs take two cells per code unit (#2665).

import { describe, expect, it } from "vitest";
import { render } from "@testing-library/react";
import { cellWidth } from "../../lib/liveTermLines";
import { Row } from "../live-terminal/TermRow";

function renderRow(texts: string[], cursorCol: number) {
  const { container } = render(<Row segs={texts.map((text) => ({ text, style: {} }))} cursorCol={cursorCol} />);
  return {
    cell: container.querySelector("[data-live-cursor]") as HTMLElement,
    spans: [...container.querySelectorAll("span")],
  };
}
const width = (cells: number) => `calc(var(--term-cell, 1em) * ${cells})`;

describe("Row cursor placement with wide characters", () => {
  it("boxes the cell right after CJK text, with the CJK stretch as one fixed box", () => {
    // 7 wide glyphs (14 cells) + "123" = 17 cells; a single box keeps bidi and shaping intact (#3342).
    const { cell, spans } = renderRow(["한글정렬테스트123"], 17);
    expect(spans.map((s) => s.textContent)).toEqual(["한글정렬테스트", "123", " "]);
    expect(spans[0]!.style.width).toBe(width(14));
    expect(cell.previousSibling!.textContent).toBe("123");
    expect(cell.style.width).toBe(width(1));
  });

  it("slices a coalesced stretch at cluster boundaries under the cursor", () => {
    const { cell, spans } = renderRow(["한글"], 2);
    expect(spans.map((s) => s.textContent)).toEqual(["한", "글"]);
    expect(spans[0]!.style.width).toBe(width(2));
    expect(cell.textContent).toBe("글");
  });

  it("keeps trailing combining marks inside the cursor cell", () => {
    const { cell, spans } = renderRow(["漢́"], 0);
    expect(cell.textContent).toBe("漢́");
    expect(cell.style.width).toBe(width(cellWidth("漢")));
    expect(spans).toHaveLength(1);
  });

  it("lands on the right glyph in a mixed ASCII and CJK line", () => {
    // "hello " is 6 cells, so column 8 is the second wide glyph.
    expect(renderRow(["hello ", "한글정렬"], 8).cell.textContent).toBe("글");
  });
});
