// @vitest-environment jsdom

import { describe, expect, it, vi } from "vitest";
import { render } from "@testing-library/react";
import { StringDiff } from "../StringDiff";

vi.mock("../../../hooks/useHighlightedLines", () => ({
  useHighlightedLines: () => ({ tokens: null }),
}));

describe("StringDiff", () => {
  it("renders both sides of an edit even without syntax tokens", () => {
    const { container } = render(
      <StringDiff oldText="const x = 1;\n" newText="const x = 42;\n" filePath="snippet.ts" />,
    );
    expect(container.textContent).toContain("const x");
    expect(container.textContent).toMatch(/42/);
    for (const span of container.querySelectorAll("span")) {
      expect(span.className).not.toMatch(/\bopacity-0\b/);
    }
  });

  it("gives the diff body a horizontal scroll context (#1568)", () => {
    // Without `overflow-x-auto` the embedding card's `overflow-hidden` clips
    // long `whitespace-pre` lines and the right side is unreachable on mobile.
    const { getByTestId } = render(
      <StringDiff oldText="const x = 1;\n" newText={`const x = ${"a".repeat(200)};\n`} filePath="snippet.ts" />,
    );
    expect(getByTestId("string-diff").className).toMatch(/\boverflow-x-auto\b/);
  });

  it("returns null for an empty diff", () => {
    const { container } = render(<StringDiff oldText="" newText="" filePath="snippet.ts" />);
    expect(container.textContent).toBe("");
  });
});
