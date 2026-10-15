// @vitest-environment jsdom

import { afterEach, describe, expect, it } from "vitest";
import { cleanup, render } from "@testing-library/react";
import { MarkdownFileView } from "../MarkdownFileView";

afterEach(cleanup);

describe("MarkdownFileView", () => {
  it("does not inject raw HTML from the file", () => {
    const md = 'text\n\n<img src=x onerror="alert(1)">\n\n<script>alert(2)</script>';
    const { container } = render(<MarkdownFileView content={md} />);
    // Without rehype-raw, react-markdown does not turn raw HTML into live DOM.
    expect(container.querySelector("script")).toBeNull();
    expect(container.querySelector("img[onerror]")).toBeNull();
    expect(container.textContent).toContain("text");
  });
});
