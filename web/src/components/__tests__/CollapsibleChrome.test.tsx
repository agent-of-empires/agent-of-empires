// @vitest-environment jsdom

import { describe, expect, it, vi } from "vitest";
import { fireEvent, render, screen } from "@testing-library/react";

import { ChromeCollapseHandle, CollapsibleRegion } from "../CollapsibleChrome";

describe("collapsible chrome", () => {
  it("marks the collapsed region inert so hidden chrome leaves the tab order", () => {
    const { rerender, container } = render(
      <CollapsibleRegion id="conversation-composer" collapsed={false}>
        <button type="button">send</button>
      </CollapsibleRegion>,
    );
    // React reflects `inert` as the DOM property on update (jsdom has no
    // native inert behavior, so read the property, not the attribute).
    const inner = () => container.firstElementChild!.firstElementChild as HTMLElement & { inert?: boolean };
    expect(inner().inert || inner().hasAttribute("inert")).toBe(false);
    rerender(
      <CollapsibleRegion id="conversation-composer" collapsed>
        <button type="button">send</button>
      </CollapsibleRegion>,
    );
    expect(inner().inert || inner().hasAttribute("inert")).toBe(true);
  });

  it("points the handle's aria-controls at the region element it collapses", () => {
    render(
      <>
        <ChromeCollapseHandle
          edge="bottom"
          collapsed={false}
          onToggle={() => {}}
          collapseLabel="Collapse message composer"
          expandLabel="Expand message composer"
          controlsId="conversation-composer"
          testId="handle"
        />
        <CollapsibleRegion id="conversation-composer" collapsed={false}>
          <button type="button">send</button>
        </CollapsibleRegion>
      </>,
    );
    const controls = screen.getByTestId("handle").getAttribute("aria-controls")!;
    // The referenced element must exist, and be the row that releases its
    // height, not the clipped child inside it.
    expect(document.getElementById(controls)).toBe(screen.getByTestId("conversation-composer"));
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
