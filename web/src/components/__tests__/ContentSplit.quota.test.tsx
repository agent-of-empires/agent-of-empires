// @vitest-environment jsdom
//
// Regression unit test for #1345 at the ContentSplit resize handle.
//
// Pre-fix, `setDiffWidth((w) => { localStorage.setItem(...); return w; })`
// ran setItem inside a React setState updater. When localStorage was full,
// the throw surfaced through React's commit phase and crashed the tree.
//
// This test mocks Storage.prototype.setItem to throw QuotaExceededError,
// mounts the component, fires the no-drag mousedown/mouseup sequence the
// user reported in #1345, and asserts the component stays rendered.

import { describe, expect, it, vi, afterEach } from "vitest";
import { cleanup, fireEvent, render } from "@testing-library/react";

import { ContentSplit } from "../ContentSplit";

afterEach(() => {
  // Vitest does not enable RTL auto-cleanup (no setupFiles in vite.config.ts).
  cleanup();
  vi.restoreAllMocks();
  window.localStorage.clear();
});

describe("ContentSplit quota crash regression (#1345)", () => {
  it("survives mousedown + mouseup when localStorage.setItem throws QuotaExceededError", () => {
    vi.spyOn(Storage.prototype, "setItem").mockImplementation(() => {
      throw new DOMException("The quota has been exceeded.", "QuotaExceededError");
    });

    const { getByTestId } = render(
      <ContentSplit
        left={<div data-testid="left">left</div>}
        right={<div data-testid="right">right</div>}
        collapsed={false}
        onToggleCollapse={() => {}}
      />,
    );

    const handle = getByTestId("content-split-resize-handle");
    // mousedown arms `dragging.current = true`, mouseup runs the persist
    // path that used to throw. With safeSetItem the throw is swallowed.
    fireEvent.mouseDown(handle);
    expect(() => fireEvent.mouseUp(document)).not.toThrow();

    // Left pane still in the DOM = React tree did not unmount.
    // ContentSplit renders `right` twice (desktop pane + mobile overlay),
    // so we assert on the unique `left` slot.
    expect(getByTestId("left")).toBeTruthy();
  });

  it("survives mouseup when localStorage.setItem throws SecurityError (private mode)", () => {
    vi.spyOn(Storage.prototype, "setItem").mockImplementation(() => {
      throw new DOMException("Storage disabled.", "SecurityError");
    });

    const { getByTestId } = render(
      <ContentSplit
        left={<div data-testid="left">left</div>}
        right={<div data-testid="right">right</div>}
        collapsed={false}
        onToggleCollapse={() => {}}
      />,
    );

    const handle = getByTestId("content-split-resize-handle");
    fireEvent.mouseDown(handle);
    expect(() => fireEvent.mouseUp(document)).not.toThrow();
    expect(getByTestId("left")).toBeTruthy();
  });
});
