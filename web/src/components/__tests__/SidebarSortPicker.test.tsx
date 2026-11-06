// @vitest-environment jsdom

import { afterEach, describe, expect, it, vi } from "vitest";
import { act, cleanup, fireEvent, render, renderHook, screen } from "@testing-library/react";

import { createRef } from "react";
import { ContextMenu } from "../ContextMenu";
import { SidebarSortPicker } from "../SidebarSortPicker";
import type { SidebarSortMode } from "../../lib/sidebarSort";
import type { Workspace } from "../../lib/types";
import { useSidebarSelection } from "../sidebar/useSidebarSelection";
import { useKeyboardShortcuts, type ShortcutActions } from "../../hooks/useKeyboardShortcuts";

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

  it("selecting another mode fires onSortModeChange and closes", () => {
    const onChange = setup("attention");
    fireEvent.click(trigger());
    fireEvent.click(option("manual"));
    expect(onChange.mock.calls).toEqual([["manual"]]);
    expect(menu()).toBeNull();
  });

  it("re-selecting the active mode closes without firing", () => {
    const onChange = setup("lastActivity");
    fireEvent.click(trigger());
    fireEvent.click(option("lastActivity"));
    expect(onChange).not.toHaveBeenCalled();
    expect(menu()).toBeNull();
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

  it("dims the trigger in manual mode and labels an unknown mode as Manual", () => {
    setup("manual");
    expect(trigger().className).toContain("text-text-dim");
    cleanup();
    setup("bogus" as SidebarSortMode);
    expect(trigger().getAttribute("aria-label")).toBe("Sort sessions, current: Manual");
  });

  // #5345: the window listener that clears the sidebar selection on Escape ran after Sort's dismissal.
  it("Escape that closes a popup keeps the sidebar selection; the next Escape clears it", () => {
    let closedMenu = () => false;
    // (popup, open it and return the element Escape is pressed on, whether it closed)
    const cases: [string, () => HTMLElement, () => boolean][] = [
      [
        "Sort",
        () => {
          setup();
          fireEvent.click(trigger());
          return option("manual");
        },
        () => menu() === null,
      ],
      // The row context menu already marks its Escape handled (ContextMenu.tsx).
      [
        "row context menu",
        () => {
          const onClose = vi.fn();
          const Menu = () => (
            <ContextMenu
              menu={{ x: 0, y: 0 }}
              menuRef={createRef<HTMLDivElement>()}
              testId="row-menu"
              onClose={onClose}
            >
              <button type="button">Rename</button>
            </ContextMenu>
          );
          render(<Menu />);
          closedMenu = () => onClose.mock.calls.length === 1;
          return screen.getByRole("button", { name: "Rename" });
        },
        () => closedMenu(),
      ],
    ];
    for (const [popup, open, closed] of cases) {
      const workspaces = [{ id: "a" }, { id: "b" }, { id: "c" }] as Workspace[];
      const { result } = renderHook(() =>
        useSidebarSelection({
          allWorkspaces: workspaces,
          orderedIds: ["a", "b", "c"],
          triage: {} as Parameters<typeof useSidebarSelection>[0]["triage"],
          activeId: null,
          onSelect: vi.fn(),
        }),
      );
      const ctrlClick = { metaKey: false, ctrlKey: true, shiftKey: false } as React.MouseEvent;
      act(() => result.current.handleRowActivate("a", ctrlClick, null));
      act(() => result.current.handleRowActivate("b", ctrlClick, null));
      const selected = () => ["a", "b", "c"].filter((id) => result.current.isSelected(id));
      expect(selected(), popup).toEqual(["a", "b"]);
      // The global Escape shortcut still sees both presses.
      const onEscape = vi.fn();
      renderHook(() => useKeyboardShortcuts(() => ({ onEscape }) as unknown as ShortcutActions));

      fireEvent.keyDown(open(), { key: "Escape" });
      expect(closed(), popup).toBe(true);
      expect(selected(), popup).toEqual(["a", "b"]);
      expect(onEscape, popup).toHaveBeenCalledTimes(1);

      fireEvent.keyDown(document.body, { key: "Escape" });
      expect(selected(), popup).toEqual([]);
      expect(onEscape, popup).toHaveBeenCalledTimes(2);
      cleanup();
    }
  });
});
