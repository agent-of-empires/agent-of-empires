// @vitest-environment jsdom
//
// FileContentViewer contract (#3088): fetches the provenance-confined /file
// endpoint and renders Markdown (rendered by default, Raw toggle) or a shiki
// full-file view for other extensions, plus the binary notice.

import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";
import { FileContentViewer } from "../FileContentViewer";
import * as api from "../../../lib/api";

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

beforeEach(() => {
  window.localStorage.clear();
});
afterEach(() => {
  cleanup();
  vi.restoreAllMocks();
  window.localStorage.clear();
});

describe("FileContentViewer", () => {
  it("renders a .md file as formatted markdown by default and toggles to raw", async () => {
    vi.spyOn(api, "getSessionFile").mockResolvedValue({
      content: "# Plan\n\nstep one",
      is_binary: false,
      truncated: false,
    });

    const { container } = render(<FileContentViewer sessionId="s1" filePath="/tmp/plan.md" />);
    await waitFor(() => {
      expect(container.querySelector("h1")?.textContent).toBe("Plan");
    });
    expect(screen.getByRole("button", { name: "Rendered" }).getAttribute("aria-pressed")).toBe("true");

    fireEvent.click(screen.getByRole("button", { name: "Raw" }));
    await waitFor(() => {
      // Raw mode renders the shiki full-file view (or its <pre> fallback), which
      // shows the literal source including the "#".
      expect(container.textContent).toContain("# Plan");
    });
    expect(container.querySelector("h1")).toBeNull();
  });

  it("renders a non-markdown file via the shiki viewer (no toggle)", async () => {
    vi.spyOn(api, "getSessionFile").mockResolvedValue({
      content: "export const a = 1;",
      is_binary: false,
      truncated: false,
    });
    const { container } = render(<FileContentViewer sessionId="s1" filePath="/repo/a.ts" />);
    await waitFor(() => {
      expect(container.textContent).toContain("export const a = 1;");
    });
    expect(screen.queryByRole("button", { name: "Rendered" })).toBeNull();
  });

  it("shows a binary notice", async () => {
    vi.spyOn(api, "getSessionFile").mockResolvedValue({
      content: "",
      is_binary: true,
      truncated: false,
    });
    render(<FileContentViewer sessionId="s1" filePath="/tmp/blob.md" />);
    await screen.findByText("Binary file");
  });

  it("shows an error when the fetch fails", async () => {
    vi.spyOn(api, "getSessionFile").mockResolvedValue(null);
    render(<FileContentViewer sessionId="s1" filePath="/tmp/x.md" />);
    await screen.findByText("Failed to load file");
  });
});
