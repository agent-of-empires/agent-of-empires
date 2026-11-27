// @vitest-environment jsdom

import { describe, expect, it, vi } from "vitest";
import { fireEvent, render, screen } from "@testing-library/react";

import { ChromeCollapseHandle, CollapsibleRegion } from "../CollapsibleChrome";

describe("collapsible chrome", () => {
  it("points the handle at its region and marks the collapsed region inert", () => {
    const tree = (collapsed: boolean) => (
      <>
        <ChromeCollapseHandle
          collapsed={collapsed}
          onToggle={() => {}}
          collapseLabel="Collapse message composer"
          expandLabel="Expand message composer"
          controlsId="conversation-composer"
          testId="handle"
        />
        <CollapsibleRegion id="conversation-composer" collapsed={collapsed}>
          <button type="button">send</button>
        </CollapsibleRegion>
      </>
    );
    const { rerender } = render(tree(false));
    const region = screen.getByTestId("conversation-composer");
    // aria-controls names the row that releases its height, not the clipped child inside it.
    expect(document.getElementById(screen.getByTestId("handle").getAttribute("aria-controls")!)).toBe(region);
    // React reflects `inert` as the DOM property on update (jsdom has no
    // native inert behavior, so read the property, not the attribute).
    const inner = () => region.firstElementChild as HTMLElement & { inert?: boolean };
    expect(inner().inert || inner().hasAttribute("inert")).toBe(false);
    rerender(tree(true));
    expect(inner().inert || inner().hasAttribute("inert")).toBe(true);
  });

  it("labels the handle for the action it performs and points the triangle at it", () => {
    // collapsed -> (accessible label, triangle points up)
    const cases = [
      [false, "Collapse message composer", false],
      [true, "Expand message composer", true],
    ] as const;
    for (const [collapsed, label, pointsUp] of cases) {
      const onToggle = vi.fn();
      const { unmount } = render(
        <ChromeCollapseHandle
          collapsed={collapsed}
          onToggle={onToggle}
          collapseLabel="Collapse message composer"
          expandLabel="Expand message composer"
          controlsId="conversation-composer"
          testId="handle"
        />,
      );
      const button = screen.getByTestId("handle");
      expect(button.getAttribute("aria-label")).toBe(label);
      expect(button.getAttribute("aria-expanded")).toBe(String(!collapsed));
      // The base glyph points up; the flipped state carries `rotate-180`.
      const flipped = button.querySelector("svg")!.getAttribute("class")!.includes("rotate-180");
      expect(flipped).toBe(!pointsUp);
      fireEvent.click(button);
      expect(onToggle).toHaveBeenCalledTimes(1);
      unmount();
    }
  });
});
