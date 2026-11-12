// @vitest-environment jsdom
import { describe, expect, it } from "vitest";
import { highlightSnippet } from "./snippetHighlighter";

describe("bundled Shiki theme integration", () => {
  it.each(["github-dark", "github-light", "dracula"])("highlights with %s through the diff engine", async (theme) => {
    const html = await highlightSnippet('const value = "<tag>";', { langHint: "typescript", theme });
    const container = document.createElement("div");
    container.innerHTML = html!;
    expect(container.querySelector("code")?.textContent).toBe('const value = "<tag>";');
    expect(container.querySelector("code tag")).toBeNull();
    expect(container.querySelector('span[style*="color:"]')).not.toBeNull();
  });
});
