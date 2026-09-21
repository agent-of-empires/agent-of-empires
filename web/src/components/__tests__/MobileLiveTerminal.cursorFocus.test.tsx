// @vitest-environment jsdom

import { describe, expect, it } from "vitest";
import { render } from "@testing-library/react";
import { Row } from "../live-terminal/TermRow";

const cursorCell = (segs: string[], cursorCol: number, focused?: boolean) =>
  render(
    <Row segs={segs.map((text) => ({ text, style: {} }))} cursorCol={cursorCol} focused={focused} />,
  ).container.querySelector("[data-live-cursor]") as HTMLElement;

describe("Row cursor cell", () => {
  it.each([undefined, false])("is a hollow, non-blinking outline when focused=%s", (focused) => {
    const cell = cursorCell(["hi"], 2, focused);
    expect(cell.style.outline).toContain("var(--term-cursor");
    expect(cell.style.backgroundColor).toBe("");
    expect(cell.className).toBe("");
  });

  it.each([
    ["on text", 1],
    ["past the row text", 5],
  ])("fills with inverted text and blinks when focused, %s", (_n, col) => {
    const cell = cursorCell(["hi"], col, true);
    expect(cell.style.backgroundColor).toContain("var(--term-cursor");
    expect(cell.style.color).toContain("var(--term-bg");
    expect(cell.style.outline).toBe("");
    expect(cell.className).toContain("animate-term-cursor-blink");
  });

  it("keeps a glued mark inside the cursor cell on a flow run (NFD input)", () => {
    const cell = cursorCell(["café"], 3);
    expect(cell.textContent).toBe("é");
    expect(cell.style.width).toBe("calc(var(--term-cell, 1em) * 1)");
    expect(cell.previousSibling!.textContent).toBe("caf");
  });

  it("boxes both the pad and the cursor on an empty row", () => {
    const cell = cursorCell([], 3);
    expect(cell.textContent).toBe(" ");
    expect(cell.style.width).toBe("calc(var(--term-cell, 1em) * 1)");
    const pad = cell.previousSibling as HTMLElement;
    expect(pad.textContent).toBe("   ");
    expect(pad.style.width).toBe("calc(var(--term-cell, 1em) * 3)");
  });
});
