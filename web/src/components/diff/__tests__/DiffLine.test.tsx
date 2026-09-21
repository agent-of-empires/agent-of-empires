// @vitest-environment jsdom

import { describe, expect, it } from "vitest";
import { render } from "@testing-library/react";
import { DiffLine } from "../DiffLine";
import type { RichDiffLine } from "../../../lib/types";

function row(type: "equal" | "add" | "delete", content: string): RichDiffLine {
  return {
    type,
    old_line_num: type === "add" ? null : 1,
    new_line_num: type === "delete" ? null : 1,
    content,
  };
}

describe("DiffLine", () => {
  it("renders raw text when no syntax tokens are provided", () => {
    const { container } = render(<DiffLine line={row("add", "const x = 42;")} />);
    expect(container.textContent).toContain("const x = 42;");
    const spans = container.querySelectorAll("span");
    for (const span of spans) {
      expect(span.className).not.toMatch(/\bopacity-0\b/);
    }
  });

  it("renders token spans with inline colors when tokens are provided", () => {
    const tokens = [
      { content: "const", color: "#f97583" },
      { content: " x = 42;", color: "#e1e4e8" },
    ];
    const { container } = render(<DiffLine line={row("add", "const x = 42;")} tokens={tokens} />);
    const colored = container.querySelectorAll("span[style*='color']");
    expect(colored.length).toBeGreaterThanOrEqual(2);
    expect(container.textContent).toContain("const x = 42;");
  });
});
