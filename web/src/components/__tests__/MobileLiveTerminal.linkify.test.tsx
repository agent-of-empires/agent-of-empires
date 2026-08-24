// @vitest-environment jsdom
//
// Row renders http(s) URLs in agent output as clickable anchors so they open
// in one tap instead of a manual select-copy-paste (#2685). Only non-cursor
// rows are linkified; the cursor (live input) row keeps its cell-split path.

import { describe, expect, it } from "vitest";
import { render } from "@testing-library/react";
import { Row } from "../MobileLiveTerminal";
import type { AnsiSegment } from "../../lib/ansi";

function seg(text: string): AnsiSegment {
  return { text, style: {} };
}

describe("Row URL linkification", () => {
  it("renders a URL in output as a new-tab anchor", () => {
    const { container } = render(<Row segs={[seg("PR: https://github.com/o/r/pull/1")]} cursorCol={null} />);
    const a = container.querySelector("a");
    expect(a).not.toBeNull();
    expect(a!.getAttribute("href")).toBe("https://github.com/o/r/pull/1");
    expect(a!.getAttribute("target")).toBe("_blank");
    expect(a!.getAttribute("rel")).toBe("noopener noreferrer");
    expect(a!.textContent).toBe("https://github.com/o/r/pull/1");
  });

  it("leaves plain output without anchors", () => {
    const { container } = render(<Row segs={[seg("no links here")]} cursorCol={null} />);
    expect(container.querySelector("a")).toBeNull();
    expect(container.textContent).toBe("no links here");
  });

  it("does not linkify the cursor row", () => {
    const { container } = render(<Row segs={[seg("https://example.com")]} cursorCol={0} />);
    expect(container.querySelector("a")).toBeNull();
  });

  it("anchors glued non-ASCII glyphs while keeping their cell boxes", () => {
    // Whole-part anchoring (#3342): the href follows the URL match
    // including the glued glyph, the glyph keeps its own fixed box inside
    // the anchor, and the row text is unchanged.
    const { container } = render(<Row segs={[seg("voir https://github.com/o/r를 suite")]} cursorCol={null} />);
    const a = container.querySelector("a");
    expect(a).not.toBeNull();
    expect(a!.getAttribute("href")).toBe("https://github.com/o/r를");
    expect(a!.textContent).toBe("https://github.com/o/r를");
    const boxed = a!.querySelector("span[style*='inline-block']");
    expect(boxed!.textContent).toBe("를");
    expect(container.textContent).toBe("voir https://github.com/o/r를 suite");
  });
});
