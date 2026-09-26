// @vitest-environment jsdom

import { describe, expect, it, vi } from "vitest";
import { fireEvent, render, screen } from "@testing-library/react";

import { ChromeCollapseHandle, CollapsibleRegion } from "../CollapsibleChrome";

describe("collapsible chrome", () => {
  it("points the handle at its region and marks the collapsed region inert", () => {
    const tree = (collapsed: boolean) => (
      <>
        <ChromeCollapseHandle
          edge="bottom"
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
    // (edge, collapsed) -> (accessible label, triangle points up)
    const cases = [
      ["top" as const, false, "Collapse conversation header", true],
      ["top" as const, true, "Expand conversation header", false],
      ["bottom" as const, false, "Collapse message composer", false],
      ["bottom" as const, true, "Expand message composer", true],
    ];
    for (const [edge, collapsed, label, pointsUp] of cases) {
      const onToggle = vi.fn();
      const { unmount } = render(
        <ChromeCollapseHandle
          edge={edge}
          collapsed={collapsed as boolean}
          onToggle={onToggle}
          collapseLabel={edge === "top" ? "Collapse conversation header" : "Collapse message composer"}
          expandLabel={edge === "top" ? "Expand conversation header" : "Expand message composer"}
          controlsId={edge === "top" ? "conversation-header" : "conversation-composer"}
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
