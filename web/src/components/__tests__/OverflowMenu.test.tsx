// @vitest-environment jsdom
//
// Coverage for OverflowMenu: the trigger toggles a menu of items, clicking an
// item fires its handler and closes the menu, and the menu closes on outside
// click and Escape. aria-expanded tracks the open state.

import { afterEach, describe, expect, it, vi } from "vitest";
import { cleanup, fireEvent, render, screen } from "@testing-library/react";

import { OverflowMenu } from "../OverflowMenu";

afterEach(cleanup);

describe("OverflowMenu", () => {
  it("opens on trigger click and lists items", () => {
    render(<OverflowMenu items={[{ label: "Rename", onClick: () => {} }]} />);
    const trigger = screen.getByRole("button", { name: "More options" });
    expect(trigger.getAttribute("aria-expanded")).toBe("false");

    fireEvent.click(trigger);
    expect(trigger.getAttribute("aria-expanded")).toBe("true");
    expect(screen.getByRole("menuitem", { name: "Rename" })).toBeTruthy();
  });

  it("fires the item handler and closes on selection", () => {
    const onClick = vi.fn();
    render(<OverflowMenu items={[{ label: "Delete", onClick }]} />);
    fireEvent.click(screen.getByRole("button", { name: "More options" }));
    fireEvent.click(screen.getByRole("menuitem", { name: "Delete" }));
    expect(onClick).toHaveBeenCalledOnce();
    expect(screen.queryByRole("menu")).toBeNull();
  });

  it("closes on outside mousedown", () => {
    render(<OverflowMenu items={[{ label: "X", onClick: () => {} }]} />);
    fireEvent.click(screen.getByRole("button", { name: "More options" }));
    expect(screen.getByRole("menu")).toBeTruthy();
    fireEvent.mouseDown(document.body);
    expect(screen.queryByRole("menu")).toBeNull();
  });

  it("closes on Escape", () => {
    render(<OverflowMenu items={[{ label: "X", onClick: () => {} }]} />);
    fireEvent.click(screen.getByRole("button", { name: "More options" }));
    fireEvent.keyDown(document, { key: "Escape" });
    expect(screen.queryByRole("menu")).toBeNull();
  });
});
