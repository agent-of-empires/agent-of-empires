// @vitest-environment jsdom

import { describe, expect, it, vi } from "vitest";
import { fireEvent, render, screen } from "@testing-library/react";

import { ChromeCollapseHandle, CollapsibleRegion } from "../CollapsibleChrome";
import { collapsibleInnerClass, collapsibleRegionClass } from "../../lib/collapsibleChrome";

describe("collapsible chrome", () => {
  it("swaps the grid row between 0fr and 1fr, and clips only while collapsed", () => {
    // Collapsing must release the row's layout height (0fr), not merely hide
    // it, and must not clip the composer's `bottom-full` menus while expanded.
    expect(collapsibleRegionClass(true)).toContain("grid-rows-[0fr]");
    expect(collapsibleRegionClass(false)).toContain("grid-rows-[1fr]");
    expect(collapsibleInnerClass(true)).toContain("overflow-hidden");
    expect(collapsibleInnerClass(false)).not.toContain("overflow-hidden");
    // `min-h-0` in both states: a grid item's automatic minimum size would
    // otherwise floor the 0fr row at the child's content height.
    expect(collapsibleInnerClass(true)).toContain("min-h-0");
    expect(collapsibleInnerClass(false)).toContain("min-h-0");
  });

  it("marks the collapsed region inert so hidden chrome leaves the tab order", () => {
    const { rerender, container } = render(
      <CollapsibleRegion collapsed={false}>
        <button type="button">send</button>
      </CollapsibleRegion>,
    );
    // React reflects `inert` as the DOM property on update (jsdom has no
    // native inert behavior, so read the property, not the attribute).
    const inner = () => container.firstElementChild!.firstElementChild as HTMLElement & { inert?: boolean };
    expect(inner().inert || inner().hasAttribute("inert")).toBe(false);
    rerender(
      <CollapsibleRegion collapsed>
        <button type="button">send</button>
      </CollapsibleRegion>,
    );
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
