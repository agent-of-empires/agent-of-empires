// @vitest-environment jsdom
import { describe, expect, it, vi } from "vitest";
import { fireEvent, render, screen } from "@testing-library/react";

import { SidebarViewMenu } from "../sidebar/SidebarViewMenu";

function setup(overrides: Partial<Parameters<typeof SidebarViewMenu>[0]> = {}) {
  const props = {
    axis: "repo" as const,
    onAxisChange: vi.fn(),
    sortMode: "manual" as const,
    onSortModeChange: vi.fn(),
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
    expect(document.activeElement).toBe(screen.getByTestId("sidebar-axis-option-repo"));
    fireEvent.click(screen.getByTestId("sidebar-axis-option-status"));
    expect(props.onAxisChange).toHaveBeenCalledWith("status");
    expect(screen.queryByTestId("sidebar-view-menu")).toBeNull();

    open();
    fireEvent.click(screen.getByTestId("sidebar-sort-option-attention"));
    open();
    fireEvent.click(screen.getByTestId("sidebar-sort-option-plugin-metric"));
    open();
    fireEvent.click(screen.getByTestId("sidebar-filter-toggle"));
    open();
    fireEvent.click(screen.getByTestId("sidebar-facet-toggle"));
    expect([
      props.onSortModeChange.mock.calls,
      props.onPluginSortChange.mock.calls,
      props.onToggleFilter.mock.calls.length,
      props.onToggleFacets.mock.calls.length,
    ]).toEqual([[["attention"]], [[{ pluginId: "p", entryId: "metric" }]], 1, 1]);
  });

  it("re-picking the current choice changes nothing", () => {
    const { props, open } = setup();
    open();
    fireEvent.click(screen.getByTestId("sidebar-axis-option-repo"));
    open();
    fireEvent.click(screen.getByTestId("sidebar-sort-option-manual"));
    expect([props.onAxisChange.mock.calls.length, props.onSortModeChange.mock.calls.length]).toEqual([0, 0]);
  });

  it("marks a non-default view with a dot and the active sort on the trigger", () => {
    setup({ sortMode: "lastActivity" });
    const trigger = screen.getByTestId("sidebar-view-toggle");
    expect([trigger.getAttribute("data-sort-mode"), trigger.querySelector(".rounded-full") !== null]).toEqual([
      "lastActivity",
      true,
    ]);
  });

  it("moves with the arrow keys and hands focus back on Escape", () => {
    const { open } = setup();
    open();
    fireEvent.keyDown(document.activeElement!, { key: "ArrowDown" });
    expect(document.activeElement).toBe(screen.getByTestId("sidebar-axis-option-org"));
    fireEvent.keyDown(document.activeElement!, { key: "Escape" });
    expect([screen.queryByTestId("sidebar-view-menu"), document.activeElement]).toEqual([
      null,
      screen.getByTestId("sidebar-view-toggle"),
    ]);
  });
});
