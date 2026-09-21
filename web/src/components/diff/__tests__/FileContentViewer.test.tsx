// @vitest-environment jsdom

import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";
import { FileContentViewer } from "../FileContentViewer";
import * as api from "../../../lib/api";

vi.mock("../../../hooks/useShikiTheme", () => ({
  useShikiTheme: () => ({ theme: "github-dark", appearance: "dark" }),
}));

// Stubbed: the whole-file view renders through `@pierre/diffs`, which needs a
// real DOM and workers. This spec is about which branch is chosen, not paint.
vi.mock("../FullFileViewer", () => ({
  FullFileViewer: ({ content, filePath }: { content: string; filePath: string }) => (
    <div data-testid="full-file" data-path={filePath}>
      {content}
    </div>
  ),
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
      // Raw mode renders the whole-file view, which shows the literal source
      // including the "#".
      expect(container.textContent).toContain("# Plan");
    });
    expect(container.querySelector("h1")).toBeNull();
  });

  it("renders a non-markdown file via the whole-file view (no toggle)", async () => {
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
