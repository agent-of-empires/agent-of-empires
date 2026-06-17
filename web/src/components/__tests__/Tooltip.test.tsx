// @vitest-environment jsdom
//
// Coverage for Tooltip (#2214): the popup must portal out to document.body so
// it escapes the sidebar scroller's `overflow-x-hidden` clip, rather than
// rendering as a nested span that gets cut off at the sidebar edge. jsdom
// cannot measure layout, so this asserts the structural fix (portaled, fixed,
// role=tooltip, shown/hidden on hover and focus) rather than geometry.

import { afterEach, describe, expect, it } from "vitest";
import { cleanup, fireEvent, render, screen } from "@testing-library/react";

import { Tooltip } from "../Tooltip";

afterEach(cleanup);

describe("Tooltip", () => {
  it("renders no tooltip until hovered", () => {
    render(
      <Tooltip text="New session">
        <button type="button">+</button>
      </Tooltip>,
    );
    expect(screen.queryByRole("tooltip")).toBeNull();
  });

  it("portals the popup to document.body on hover and removes it on leave", () => {
    const { container } = render(
      <Tooltip text="New session">
        <button type="button">+</button>
      </Tooltip>,
    );
    const trigger = screen.getByRole("button").parentElement!;

    fireEvent.mouseEnter(trigger);

    const tooltip = screen.getByRole("tooltip");
    expect(tooltip.textContent).toBe("New session");
    // Portaled directly under body, not nested inside the trigger / render tree.
    expect(tooltip.parentElement).toBe(document.body);
    expect(container.contains(tooltip)).toBe(false);
    // Fixed positioning is what lets it escape the overflow ancestor.
    expect(tooltip.classList.contains("fixed")).toBe(true);

    fireEvent.mouseLeave(trigger);
    expect(screen.queryByRole("tooltip")).toBeNull();
  });

  it("opens on focus and closes on blur", () => {
    render(
      <Tooltip text="New session">
        <button type="button">+</button>
      </Tooltip>,
    );
    const button = screen.getByRole("button");

    fireEvent.focus(button);
    expect(screen.queryByRole("tooltip")).not.toBeNull();

    fireEvent.blur(button);
    expect(screen.queryByRole("tooltip")).toBeNull();
  });
});
