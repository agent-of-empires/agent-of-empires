// @vitest-environment jsdom
//
// MarkdownFileView contract (#3088): renders a Markdown file to formatted
// HTML. Verifies it
//   - renders standard Markdown (headings, lists, links, GFM tables),
//   - escapes raw HTML rather than injecting it (no rehype-raw / no
//     dangerouslySetInnerHTML), so a hostile file cannot run script.

import { afterEach, describe, expect, it } from "vitest";
import { cleanup, render } from "@testing-library/react";
import { MarkdownFileView } from "../MarkdownFileView";

afterEach(cleanup);

describe("MarkdownFileView", () => {
  it("renders headings, lists, and links", () => {
    const md = ["# Title", "", "- one", "- two", "", "[link](https://example.com)"].join("\n");
    const { container } = render(<MarkdownFileView content={md} />);
    expect(container.querySelector("h1")?.textContent).toBe("Title");
    expect(container.querySelectorAll("li")).toHaveLength(2);
    const a = container.querySelector("a");
    expect(a?.getAttribute("href")).toBe("https://example.com");
    expect(a?.textContent).toBe("link");
  });

  it("renders GFM tables via remark-gfm", () => {
    const md = ["| a | b |", "| - | - |", "| 1 | 2 |"].join("\n");
    const { container } = render(<MarkdownFileView content={md} />);
    expect(container.querySelector("table")).toBeTruthy();
    expect(container.querySelectorAll("td")).toHaveLength(2);
  });

  it("does not inject raw HTML from the file", () => {
    const md = 'text\n\n<img src=x onerror="alert(1)">\n\n<script>alert(2)</script>';
    const { container } = render(<MarkdownFileView content={md} />);
    // Without rehype-raw, react-markdown does not turn raw HTML into live DOM.
    expect(container.querySelector("script")).toBeNull();
    expect(container.querySelector("img[onerror]")).toBeNull();
    expect(container.textContent).toContain("text");
  });
});
