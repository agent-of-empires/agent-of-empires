// @vitest-environment jsdom

import { afterEach, describe, expect, it, vi } from "vitest";
import { cleanup, fireEvent, render, screen } from "@testing-library/react";

import { Tooltip } from "../Tooltip";

afterEach(cleanup);

describe("Tooltip", () => {
  it("portals the popup to document.body on hover and removes it on leave", () => {
    const { container } = render(
      <Tooltip text="New session">
        <button type="button">+</button>
      </Tooltip>,
    );
    const trigger = screen.getByRole("button").parentElement!;
    expect(screen.queryByRole("tooltip")).toBeNull();

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

  it("describes its trigger with the bubble while open, and a controlled one ignores hover", () => {
    const { rerender } = render(
      <Tooltip text="New session">
        <button type="button">+</button>
      </Tooltip>,
    );
    const button = screen.getByRole("button");
    expect(button.getAttribute("aria-describedby")).toBeNull();
    fireEvent.focus(button);
    const tip = screen.getByRole("tooltip");
    expect(tip.id).not.toBe("");
    expect(button.getAttribute("aria-describedby")).toBe(tip.id);
    fireEvent.blur(button);
    expect(button.getAttribute("aria-describedby")).toBeNull();

    // Controlled: the caller decides when it shows, and describes its own focus target.
    rerender(
      <Tooltip text="Waiting: the Agent needs your input" id="glyph-tip" open={false}>
        <span>⠃</span>
      </Tooltip>,
    );
    fireEvent.mouseEnter(screen.getByText("⠃").parentElement!);
    expect(screen.queryByRole("tooltip")).toBeNull();
    rerender(
      <Tooltip text="Waiting: the Agent needs your input" id="glyph-tip" open>
        <span>⠃</span>
      </Tooltip>,
    );
    expect(screen.getByRole("tooltip").id).toBe("glyph-tip");
    expect(screen.getByText("⠃").getAttribute("aria-describedby")).toBeNull();
    // A controlled tooltip follows a scrolling anchor rather than dismissing itself.
    const anchor = screen.getByText("⠃").parentElement!;
    const at = (top: number) => vi.spyOn(anchor, "getBoundingClientRect").mockReturnValue(new DOMRect(40, top, 10, 10));
    at(100);
    fireEvent.scroll(window);
    expect(screen.getByRole("tooltip").style.top).toBe("116px");
    at(200);
    fireEvent.scroll(window);
    expect(screen.getByRole("tooltip").style.top).toBe("216px");
    // A touch or pen on a controlled tooltip's trigger leaves it to its caller: it stays visible.
    for (const pointerType of ["touch", "pen"]) {
      fireEvent.pointerDown(anchor, { pointerType });
      expect(screen.getByRole("tooltip").style.visibility, pointerType).toBe("visible");
    }
    // Back to a mouse, as the next part of this test assumes.
    fireEvent.pointerDown(anchor, { pointerType: "mouse" });

    // A child's own description is kept alongside the bubble.
    rerender(
      <Tooltip text="New session">
        <button type="button" aria-describedby="hint">
          +
        </button>
      </Tooltip>,
    );
    fireEvent.focus(screen.getByRole("button"));
    expect(screen.getByRole("button").getAttribute("aria-describedby")).toBe(`hint ${screen.getByRole("tooltip").id}`);
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

  it("stays closed through the emulated hover and focus of a tap, and opens again for a mouse", () => {
    render(
      <Tooltip text="New session">
        <button type="button">+</button>
      </Tooltip>,
    );
    const button = screen.getByRole("button");
    const trigger = button.parentElement!;

    fireEvent.pointerDown(button, { pointerType: "touch" });
    fireEvent.mouseEnter(trigger);
    fireEvent.focus(button);
    // Present at all, visible or not: a re-shown tooltip is hidden until it re-measures.
    expect(document.querySelector("[role=tooltip]")).toBeNull();

    fireEvent.blur(button);
    fireEvent.pointerEnter(trigger, { pointerType: "mouse" });
    fireEvent.mouseEnter(trigger);
    expect(document.querySelector("[role=tooltip]")).not.toBeNull();
  });

  it("closes a tooltip a touch entry event opened before the tap's pointerdown", () => {
    render(
      <Tooltip text="New session">
        <button type="button">+</button>
      </Tooltip>,
    );
    const button = screen.getByRole("button");
    fireEvent.mouseEnter(button.parentElement!);
    expect(document.querySelector("[role=tooltip]")).not.toBeNull();
    fireEvent.pointerDown(button, { pointerType: "touch" });
    expect(document.querySelector("[role=tooltip]")).toBeNull();
  });
});
