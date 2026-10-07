// @vitest-environment jsdom
// Output URLs render as new-tab anchors (#2685); the cursor row is not linkified.

import { describe, expect, it } from "vitest";
import { render } from "@testing-library/react";
import { Row } from "../live-terminal/TermRow";

const renderRow = (text: string, cursorCol: number | null = null) =>
  render(<Row segs={[{ text, style: {} }]} cursorCol={cursorCol} />).container;

describe("Row URL linkification", () => {
  it("renders a URL in output as a new-tab anchor", () => {
    const a = renderRow("PR: https://github.com/o/r/pull/1").querySelector("a")!;
    expect(a.getAttribute("href")).toBe("https://github.com/o/r/pull/1");
    expect(a.getAttribute("target")).toBe("_blank");
    expect(a.getAttribute("rel")).toBe("noopener noreferrer");
    expect(a.textContent).toBe("https://github.com/o/r/pull/1");
  });

  it.each([["the cursor row", "https://example.com", 0]])("leaves %s without anchors", (_n, text, cursorCol) => {
    const container = renderRow(text, cursorCol);
    expect(container.querySelector("a")).toBeNull();
    expect(container.textContent).toContain(text);
  });

  it("anchors glued non-ASCII glyphs while keeping their cell boxes (#3342)", () => {
    const container = renderRow("voir https://github.com/o/r를 suite");
    const a = container.querySelector("a")!;
    expect(a.getAttribute("href")).toBe("https://github.com/o/r를");
    expect(a.textContent).toBe("https://github.com/o/r를");
    expect(a.querySelector("span[style*='inline-block']")!.textContent).toBe("를");
    expect(container.textContent).toBe("voir https://github.com/o/r를 suite");
  });
});
