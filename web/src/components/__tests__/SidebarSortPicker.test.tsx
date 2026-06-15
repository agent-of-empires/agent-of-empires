// @vitest-environment jsdom
//
// Tests for SidebarSortPicker: the labeled dropdown that selects one of the
// three sidebar sort modes (#1640). Cover the open/close toggle, selecting
// each mode (and the callback payload), the no-op when re-selecting the
// active mode, the brand tint for non-manual modes, and outside-click /
// Escape close.

import { afterEach, describe, expect, it, vi } from "vitest";
import { cleanup, fireEvent, render } from "@testing-library/react";

import { SidebarSortPicker } from "../SidebarSortPicker";
import type { SidebarSortMode } from "../../lib/sidebarSort";

function setup(sortMode: SidebarSortMode = "manual") {
  const onSortModeChange = vi.fn();
  const utils = render(<SidebarSortPicker sortMode={sortMode} onSortModeChange={onSortModeChange} />);
  return { ...utils, onSortModeChange };
}

function toggle(container: HTMLElement): HTMLButtonElement {
  return container.querySelector<HTMLButtonElement>('[data-testid="sidebar-sort-toggle"]')!;
}

afterEach(() => {
  cleanup();
});

describe("SidebarSortPicker", () => {
  it("renders the trigger reflecting the active mode and starts closed", () => {
    const { container } = setup("lastActivity");
    const trigger = toggle(container);
    expect(trigger.getAttribute("data-sort-mode")).toBe("lastActivity");
    expect(trigger.getAttribute("aria-expanded")).toBe("false");
    expect(trigger.getAttribute("aria-label")).toBe("Sort sessions, current: Last activity");
    expect(container.querySelector('[data-testid="sidebar-sort-menu"]')).toBeNull();
  });

  it("opens the menu on trigger click and lists all three modes", () => {
    const { container } = setup("manual");
    fireEvent.click(toggle(container));
    expect(toggle(container).getAttribute("aria-expanded")).toBe("true");
    expect(container.querySelector('[data-testid="sidebar-sort-menu"]')).toBeTruthy();
    expect(container.querySelector('[data-testid="sidebar-sort-option-manual"]')).toBeTruthy();
    expect(container.querySelector('[data-testid="sidebar-sort-option-lastActivity"]')).toBeTruthy();
    expect(container.querySelector('[data-testid="sidebar-sort-option-attention"]')).toBeTruthy();
  });

  it("marks the active option as checked", () => {
    const { container } = setup("attention");
    fireEvent.click(toggle(container));
    const attention = container.querySelector('[data-testid="sidebar-sort-option-attention"]')!;
    expect(attention.getAttribute("aria-checked")).toBe("true");
    const manual = container.querySelector('[data-testid="sidebar-sort-option-manual"]')!;
    expect(manual.getAttribute("aria-checked")).toBe("false");
  });

  it("clicking a toggle a second time closes the menu", () => {
    const { container } = setup();
    fireEvent.click(toggle(container));
    expect(container.querySelector('[data-testid="sidebar-sort-menu"]')).toBeTruthy();
    fireEvent.click(toggle(container));
    expect(container.querySelector('[data-testid="sidebar-sort-menu"]')).toBeNull();
  });

  it.each<[SidebarSortMode, SidebarSortMode]>([
    ["manual", "lastActivity"],
    ["manual", "attention"],
    ["lastActivity", "manual"],
    ["lastActivity", "attention"],
    ["attention", "manual"],
    ["attention", "lastActivity"],
  ])("from %s selecting %s fires onSortModeChange and closes the menu", (current, next) => {
    const { container, onSortModeChange } = setup(current);
    fireEvent.click(toggle(container));
    fireEvent.click(container.querySelector(`[data-testid="sidebar-sort-option-${next}"]`)!);
    expect(onSortModeChange).toHaveBeenCalledTimes(1);
    expect(onSortModeChange).toHaveBeenCalledWith(next);
    expect(container.querySelector('[data-testid="sidebar-sort-menu"]')).toBeNull();
  });

  it("re-selecting the already-active mode closes the menu without firing the callback", () => {
    const { container, onSortModeChange } = setup("lastActivity");
    fireEvent.click(toggle(container));
    fireEvent.click(container.querySelector('[data-testid="sidebar-sort-option-lastActivity"]')!);
    expect(onSortModeChange).not.toHaveBeenCalled();
    expect(container.querySelector('[data-testid="sidebar-sort-menu"]')).toBeNull();
  });

  it("dims the trigger in manual mode", () => {
    // Only the manual-mode dim is asserted; the non-manual chrome color is
    // left to the design system (brand amber is reserved for cursor / active
    // border / focus rings, not general chrome), so it is not pinned here.
    const { container: manualC } = setup("manual");
    expect(toggle(manualC).className).toContain("text-text-dim");
  });

  it("closes on an outside mousedown", () => {
    const { container } = setup();
    fireEvent.click(toggle(container));
    expect(container.querySelector('[data-testid="sidebar-sort-menu"]')).toBeTruthy();
    fireEvent.mouseDown(document.body);
    expect(container.querySelector('[data-testid="sidebar-sort-menu"]')).toBeNull();
  });

  it("does not close on a mousedown inside the component", () => {
    const { container } = setup();
    fireEvent.click(toggle(container));
    fireEvent.mouseDown(container.querySelector('[data-testid="sidebar-sort-menu"]')!);
    expect(container.querySelector('[data-testid="sidebar-sort-menu"]')).toBeTruthy();
  });

  it("closes on Escape", () => {
    const { container } = setup();
    fireEvent.click(toggle(container));
    expect(container.querySelector('[data-testid="sidebar-sort-menu"]')).toBeTruthy();
    fireEvent.keyDown(document, { key: "Escape" });
    expect(container.querySelector('[data-testid="sidebar-sort-menu"]')).toBeNull();
  });

  it("falls back to the Manual spec for an unknown sort mode", () => {
    // The component guards with `?? MODES[0]`; an off-spec value should not crash
    // and should render the manual trigger label.
    const { container } = setup("bogus" as SidebarSortMode);
    expect(toggle(container).getAttribute("aria-label")).toBe("Sort sessions, current: Manual");
  });
});
