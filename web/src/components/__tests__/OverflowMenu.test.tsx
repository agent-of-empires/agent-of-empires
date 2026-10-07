// @vitest-environment jsdom

import { afterEach, describe, expect, it, vi } from "vitest";
import { cleanup, fireEvent, render, screen } from "@testing-library/react";

import { OverflowMenu } from "../OverflowMenu";

afterEach(cleanup);

describe("OverflowMenu", () => {
  it("opens on trigger click, fires the item handler, and closes on selection", () => {
    const onClick = vi.fn();
    render(<OverflowMenu items={[{ label: "Delete", onClick }]} />);
    const trigger = screen.getByRole("button", { name: "More options" });
    expect(trigger.getAttribute("aria-expanded")).toBe("false");
    fireEvent.click(trigger);
    expect(trigger.getAttribute("aria-expanded")).toBe("true");
    fireEvent.click(screen.getByRole("menuitem", { name: "Delete" }));
    expect(onClick).toHaveBeenCalledOnce();
    expect(screen.queryByRole("menu")).toBeNull();
  });

  it("closes on outside mousedown and on Escape", () => {
    render(<OverflowMenu items={[{ label: "X", onClick: () => {} }]} />);
    fireEvent.click(screen.getByRole("button", { name: "More options" }));
    expect(screen.getByRole("menu")).toBeTruthy();
    fireEvent.mouseDown(document.body);
    expect(screen.queryByRole("menu")).toBeNull();
    fireEvent.click(screen.getByRole("button", { name: "More options" }));
    fireEvent.keyDown(document, { key: "Escape" });
    expect(screen.queryByRole("menu")).toBeNull();
  });
});
