// @vitest-environment jsdom
//
// FullFileViewer contract (#1810): the full-file fallback shown when an
// agent-cited file has no diff against the base. Verifies it
//   - highlights the file via the shared shiki highlighter for a known
//     language and injects the resulting markup,
//   - falls back to a plain <pre> with the raw content for an unknown
//     extension (highlighter never runs),
//   - drops stale highlighted markup when the rendered file changes, so a
//     switch can't keep painting the previous file's html.

import { afterEach, describe, expect, it, vi } from "vitest";
import { cleanup, render, waitFor } from "@testing-library/react";
import { FullFileViewer } from "../FullFileViewer";

vi.mock("../../../hooks/useShikiTheme", () => ({
  useShikiTheme: () => ({ theme: "github-dark", appearance: "dark" }),
}));

vi.mock("../../../lib/highlighter", () => ({
  ensureThemeLoaded: vi.fn().mockResolvedValue("github-dark"),
  getHighlighter: vi.fn().mockResolvedValue({
    codeToHtml: (code: string) => `<pre class="shiki"><code>${code}</code></pre>`,
  }),
  langKeyForExt: (s: string) => s,
  loadLanguage: vi.fn().mockResolvedValue(undefined),
}));

afterEach(cleanup);

describe("FullFileViewer", () => {
  it("highlights a known-language file and injects the markup", async () => {
    const { container } = render(<FullFileViewer content="export const a = 1;\n" filePath="src/a.ts" />);
    await waitFor(() => {
      expect(container.querySelector("pre.shiki")).toBeTruthy();
    });
    expect(container.textContent).toContain("export const a = 1;");
  });

  it("renders a plain pre for an unknown extension", async () => {
    const { container } = render(<FullFileViewer content="just text" filePath="notes.unknownext" />);
    // No grammar resolves, so the highlighter never produces markup.
    await waitFor(() => {
      expect(container.querySelector("pre")).toBeTruthy();
    });
    expect(container.querySelector("pre.shiki")).toBeNull();
    expect(container.textContent).toContain("just text");
  });

  it("drops stale highlighted markup when the file changes", async () => {
    const { container, rerender } = render(<FullFileViewer content="export const a = 1;\n" filePath="src/a.ts" />);
    await waitFor(() => {
      expect(container.querySelector("pre.shiki")).toBeTruthy();
    });
    // Switch to an unknown-language file: the retained markup must clear so the
    // viewer doesn't paint the previous file's contents.
    rerender(<FullFileViewer content="plain b" filePath="src/b.unknownext" />);
    expect(container.querySelector("pre.shiki")).toBeNull();
    expect(container.textContent).toContain("plain b");
  });
});
