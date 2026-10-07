// @vitest-environment jsdom

import { afterEach, describe, expect, it, vi } from "vitest";
import { fireEvent, render } from "@testing-library/react";

import { ContentSplit } from "../ContentSplit";

afterEach(() => {
  vi.restoreAllMocks();
  window.localStorage.clear();
});

describe("ContentSplit quota crash regression (#1345)", () => {
  it("survives mousedown + mouseup when localStorage.setItem throws (quota or private mode)", () => {
    const setItem = vi.spyOn(Storage.prototype, "setItem");
    for (const name of ["QuotaExceededError", "SecurityError"]) {
      setItem.mockImplementation(() => {
        throw new DOMException("Storage unavailable.", name);
      });
      const { getByTestId, unmount } = render(
        <ContentSplit
          left={<div data-testid="left">left</div>}
          right={<div data-testid="right">right</div>}
          collapsed={false}
          onToggleCollapse={() => {}}
        />,
      );

      // mousedown arms `dragging.current = true`, mouseup runs the persist
      // path that used to throw. With safeSetItem the throw is swallowed.
      fireEvent.mouseDown(getByTestId("content-split-resize-handle"));
      expect(() => fireEvent.mouseUp(document)).not.toThrow();
      // Left pane still in the DOM = React tree did not unmount.
      expect(getByTestId("left")).toBeTruthy();
      unmount();
    }
  });
});
