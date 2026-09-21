// @vitest-environment jsdom

import { afterEach, describe, expect, it, vi } from "vitest";
import { cleanup, fireEvent, render, screen } from "@testing-library/react";

import { SidebarSortPicker } from "../SidebarSortPicker";
import type { SidebarSortMode } from "../../lib/sidebarSort";

function setup(sortMode: SidebarSortMode = "manual") {
  const onSortModeChange = vi.fn();
  render(<SidebarSortPicker sortMode={sortMode} onSortModeChange={onSortModeChange} />);
  return onSortModeChange;
}
const trigger = () => screen.getByTestId("sidebar-sort-toggle");
const menu = () => screen.queryByTestId("sidebar-sort-menu");
const option = (mode: string) => screen.getByTestId(`sidebar-sort-option-${mode}`);

afterEach(cleanup);

describe("SidebarSortPicker", () => {
  it("starts closed with a trigger reflecting the mode, and toggles the menu", () => {
    setup("lastActivity");
    expect(trigger().getAttribute("data-sort-mode")).toBe("lastActivity");
    expect(trigger().getAttribute("aria-label")).toBe("Sort sessions, current: Last activity");
    expect(trigger().getAttribute("aria-expanded")).toBe("false");
    expect(menu()).toBeNull();
    fireEvent.click(trigger());
    expect(trigger().getAttribute("aria-expanded")).toBe("true");
    expect(option("lastActivity").getAttribute("aria-checked")).toBe("true");
    expect(option("manual").getAttribute("aria-checked")).toBe("false");
    expect(option("attention")).not.toBeNull();
    fireEvent.click(trigger());
    expect(menu()).toBeNull();
  });

  it.each<[SidebarSortMode, SidebarSortMode]>([
    ["manual", "lastActivity"],
    ["manual", "attention"],
    ["lastActivity", "manual"],
    ["lastActivity", "attention"],
    ["attention", "manual"],
    ["attention", "lastActivity"],
  ])("from %s selecting %s fires onSortModeChange and closes", (current, next) => {
    const onChange = setup(current);
    fireEvent.click(trigger());
    fireEvent.click(option(next));
    expect(onChange.mock.calls).toEqual([[next]]);
    expect(menu()).toBeNull();
  });

  it("re-selecting the active mode closes without firing", () => {
    const onChange = setup("lastActivity");
    fireEvent.click(trigger());
    fireEvent.click(option("lastActivity"));
    expect(onChange).not.toHaveBeenCalled();
    expect(menu()).toBeNull();
  });

  it("dims the trigger in manual mode", () => {
    setup("manual");
    expect(trigger().className).toContain("text-text-dim");
  });

  it("closes on an outside mousedown or Escape, not an inside mousedown", () => {
    setup();
    fireEvent.click(trigger());
    fireEvent.mouseDown(menu()!);
    expect(menu()).not.toBeNull();
    fireEvent.mouseDown(document.body);
    expect(menu()).toBeNull();
    fireEvent.click(trigger());
    fireEvent.keyDown(document, { key: "Escape" });
    expect(menu()).toBeNull();
  });

  it("falls back to Manual for an unknown mode", () => {
    setup("bogus" as SidebarSortMode);
    expect(trigger().getAttribute("aria-label")).toBe("Sort sessions, current: Manual");
  });
});
