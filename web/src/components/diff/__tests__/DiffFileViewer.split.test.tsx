// @vitest-environment jsdom
//
// Covers the unified/split toggle in DiffFileViewer, its localStorage
// persistence via useWebSettings, that the width ResizeObserver attaches even
// when the diff container mounts after an initial loading phase, and that the
// selected layout is forwarded to the Pierre renderer as `options.diffStyle`.
//
// The Pierre renderer (`@pierre/diffs/react`) manipulates the DOM and spins up
// workers, neither of which runs under jsdom, so it's mocked here with a light
// stand-in that surfaces the diffStyle it was handed. Round-trip rendering is
// covered by the live Playwright suite instead.

import { fireEvent, render, screen, waitFor } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { DiffFileViewer } from "../DiffFileViewer";
import type { RichFileContentsResponse } from "../../../lib/types";

const contents: RichFileContentsResponse = {
  file: {
    path: "a.ts",
    old_path: null,
    status: "modified",
    additions: 1,
    deletions: 1,
  },
  old_content: "ctx\nold\n",
  new_content: "ctx\nnew\n",
  // Server-computed unified diff (similar-crate format).
  patch: "--- a/a.ts\n+++ b/a.ts\n@@ -1,2 +1,2 @@\n ctx\n-old\n+new\n",
  is_binary: false,
  truncated: false,
};

const mock = vi.hoisted(() => ({
  contents: undefined as RichFileContentsResponse | undefined,
  observe: vi.fn(),
}));

vi.mock("../../../hooks/useFileContents", () => ({
  useFileContents: () => ({
    contents: mock.contents,
    loading: mock.contents === undefined,
    error: null,
    refresh: vi.fn(),
  }),
}));

// Stand in for the Pierre renderer: surface the diffStyle on a data attribute
// and render the file name so the header assertions still resolve.
vi.mock("@pierre/diffs/react", () => ({
  FileDiff: ({
    options,
    fileDiff,
  }: {
    options: { diffStyle: string };
    fileDiff: { name: string };
  }) => (
    <div data-testid="pierre-diff" data-diff-style={options.diffStyle}>
      {fileDiff.name}
    </div>
  ),
  Virtualizer: ({ children }: { children: React.ReactNode }) => (
    <div data-testid="virtualizer">{children}</div>
  ),
  WorkerPoolContextProvider: ({ children }: { children: React.ReactNode }) => (
    <>{children}</>
  ),
}));

beforeEach(() => {
  window.localStorage.clear();
  mock.contents = contents;
  mock.observe.mockClear();
  class WideRO {
    cb: ResizeObserverCallback;
    constructor(cb: ResizeObserverCallback) {
      this.cb = cb;
    }
    observe(el: Element) {
      mock.observe(el);
      this.cb(
        [{ contentRect: { width: 1000 } } as ResizeObserverEntry],
        this as unknown as ResizeObserver,
      );
    }
    unobserve() {}
    disconnect() {}
  }
  vi.stubGlobal("ResizeObserver", WideRO);
});

afterEach(() => {
  vi.unstubAllGlobals();
  window.localStorage.clear();
});

describe("DiffFileViewer split layout", () => {
  it("defaults to unified (Split toggle not pressed)", async () => {
    render(<DiffFileViewer sessionId="s1" filePath="a.ts" />);
    await screen.findByText(/Modified/i);
    expect(
      screen
        .getByRole("button", { name: "Split" })
        .getAttribute("aria-pressed"),
    ).toBe("false");
    expect(
      screen.getByTestId("pierre-diff").getAttribute("data-diff-style"),
    ).toBe("unified");
  });

  it("switches to split, forwards diffStyle, and persists the preference", async () => {
    render(<DiffFileViewer sessionId="s1" filePath="a.ts" />);
    await screen.findByText(/Modified/i);

    fireEvent.click(screen.getByRole("button", { name: "Split" }));

    await waitFor(() => {
      expect(
        screen
          .getByRole("button", { name: "Split" })
          .getAttribute("aria-pressed"),
      ).toBe("true");
    });
    expect(
      screen.getByTestId("pierre-diff").getAttribute("data-diff-style"),
    ).toBe("split");
    expect(
      JSON.parse(window.localStorage.getItem("aoe-web-settings") ?? "{}")
        .diffViewLayout,
    ).toBe("split");
  });

  it("attaches the width observer when the diff container mounts after loading", async () => {
    mock.contents = undefined;
    const { rerender } = render(
      <DiffFileViewer sessionId="s1" filePath="a.ts" />,
    );
    expect(mock.observe).not.toHaveBeenCalled();

    mock.contents = contents;
    rerender(<DiffFileViewer sessionId="s1" filePath="a.ts" />);
    await screen.findByText(/Modified/i);
    expect(mock.observe).toHaveBeenCalled();
  });
});
