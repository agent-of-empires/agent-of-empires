// @vitest-environment jsdom
import { afterEach, describe, expect, it, vi } from "vitest";
import { act, cleanup, fireEvent, render, renderHook, screen } from "@testing-library/react";

import { createRef } from "react";
import { ContextMenu } from "../ContextMenu";
import { SidebarViewMenu } from "../sidebar/SidebarViewMenu";
import { useSidebarSelection } from "../sidebar/useSidebarSelection";
import { makeSession, makeWorkspace } from "./fixtures";
import { useKeyboardShortcuts, type ShortcutActions } from "../../hooks/useKeyboardShortcuts";

afterEach(cleanup);

function setup(overrides: Partial<Parameters<typeof SidebarViewMenu>[0]> = {}) {
  const props = {
    groupBy: "project" as const,
    onGroupByChange: vi.fn(),
    sort: "custom" as const,
    onSortChange: vi.fn(),
    pluginSorts: [{ pluginId: "p", entryId: "metric", label: "By metric" }] as never,
    pluginSortRef: null,
    onPluginSortChange: vi.fn(),
    filterOpen: false,
    onToggleFilter: vi.fn(),
    hasFacets: true,
    facetsActive: false,
    facetOpen: false,
    onToggleFacets: vi.fn(),
    ...overrides,
  };
  render(<SidebarViewMenu {...props} />);
  const open = () => fireEvent.click(screen.getByTestId("sidebar-view-toggle"));
  return { props, open };
}

describe("SidebarViewMenu", () => {
  it("groups, sorts, filters and shows facets from one menu, closing on each choice", () => {
    const { props, open } = setup();
    open();
    expect(document.activeElement).toBe(screen.getByTestId("sidebar-view-group-project"));
    fireEvent.click(screen.getByTestId("sidebar-view-group-group"));
    expect(props.onGroupByChange).toHaveBeenCalledWith("group");
    expect(screen.queryByTestId("sidebar-view-menu")).toBeNull();

    open();
    fireEvent.click(screen.getByTestId("sidebar-view-sort-newest"));
    open();
    fireEvent.click(screen.getByTestId("sidebar-sort-option-plugin-metric"));
    open();
    fireEvent.click(screen.getByTestId("sidebar-filter-toggle"));
    open();
    fireEvent.click(screen.getByTestId("sidebar-facet-toggle"));
    expect([
      props.onSortChange.mock.calls,
      props.onPluginSortChange.mock.calls,
      props.onToggleFilter.mock.calls.length,
      props.onToggleFacets.mock.calls.length,
    ]).toEqual([[["newest"]], [[{ pluginId: "p", entryId: "metric" }]], 1, 1]);
  });

  it("re-picking the current choice changes nothing", () => {
    const { props, open } = setup();
    open();
    fireEvent.click(screen.getByTestId("sidebar-view-group-project"));
    open();
    fireEvent.click(screen.getByTestId("sidebar-view-sort-custom"));
    expect([props.onGroupByChange.mock.calls.length, props.onSortChange.mock.calls.length]).toEqual([0, 0]);
  });

  it("marks a non-default view with a dot and the active sort on the trigger", () => {
    for (const [overrides, sort, dot] of [
      [{ sort: "newest" }, "newest", true],
      [{}, "custom", false],
      [{ filterActive: true }, "custom", true],
    ] as const) {
      setup(overrides);
      const trigger = screen.getByTestId("sidebar-view-toggle");
      expect([trigger.getAttribute("data-sort"), trigger.querySelector(".rounded-full") !== null]).toEqual([sort, dot]);
      cleanup();
    }
  });

  it("lets a viewer who cannot change the view drop a plugin sort without changing the shared sort", () => {
    const { props, open } = setup({ locked: true, pluginSortRef: { pluginId: "p", entryId: "metric" } });
    open();
    expect((screen.getByTestId("sidebar-view-sort-name") as HTMLButtonElement).disabled).toBe(true);
    expect((screen.getByTestId("sidebar-view-group-group") as HTMLButtonElement).disabled).toBe(true);
    fireEvent.click(screen.getByTestId("sidebar-view-sort-custom"));
    expect(props.onSortChange).toHaveBeenCalledWith("custom");
  });

  it("moves with the arrow keys and hands focus back on Escape", () => {
    const { open } = setup();
    open();
    fireEvent.keyDown(document.activeElement!, { key: "ArrowDown" });
    expect(document.activeElement).toBe(screen.getByTestId("sidebar-view-group-group"));
    fireEvent.keyDown(document.activeElement!, { key: "Escape" });
    expect([screen.queryByTestId("sidebar-view-menu"), document.activeElement]).toEqual([
      null,
      screen.getByTestId("sidebar-view-toggle"),
    ]);
  });

  // #5345: the window listener that clears the sidebar selection on Escape ran after the view menu's dismissal.
  it("Escape that closes a popup keeps the sidebar selection; the next Escape clears it", () => {
    let closedMenu = () => false;
    // (popup, open it and return the element Escape is pressed on, whether it closed, whether the global Escape
    // shortcut sees the press that closes it)
    const cases: [string, () => HTMLElement, () => boolean, number][] = [
      [
        "View",
        () => {
          setup().open();
          return screen.getByTestId("sidebar-view-sort-name");
        },
        () => screen.queryByTestId("sidebar-view-menu") === null,
        // The View options menu keeps the Escape that closes it.
        0,
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
        1,
      ],
    ];
    for (const [popup, open, closed, seen] of cases) {
      const rows = ["a", "b", "c"].map((id) => ({
        key: id,
        workspace: makeWorkspace(id, [makeSession({ id: `s-${id}` })]),
      }));
      const { result } = renderHook(() =>
        useSidebarSelection({
          rows,
          orderedIds: ["a", "b", "c"],
          triage: {} as Parameters<typeof useSidebarSelection>[0]["triage"],
          activeId: null,
          activeSessionId: null,
          onSelect: vi.fn(),
        }),
      );
      const ctrlClick = { metaKey: false, ctrlKey: true, shiftKey: false } as React.MouseEvent;
      act(() => result.current.handleRowActivate("a", ctrlClick, null));
      act(() => result.current.handleRowActivate("b", ctrlClick, null));
      const selected = () => ["a", "b", "c"].filter((id) => result.current.isSelected(id));
      expect(selected(), popup).toEqual(["a", "b"]);
      const onEscape = vi.fn();
      renderHook(() => useKeyboardShortcuts(() => ({ onEscape }) as unknown as ShortcutActions));

      fireEvent.keyDown(open(), { key: "Escape" });
      expect(closed(), popup).toBe(true);
      expect(selected(), popup).toEqual(["a", "b"]);
      expect(onEscape, popup).toHaveBeenCalledTimes(seen);

      fireEvent.keyDown(document.body, { key: "Escape" });
      expect(selected(), popup).toEqual([]);
      expect(onEscape, popup).toHaveBeenCalledTimes(seen + 1);
      cleanup();
    }
  });
});
