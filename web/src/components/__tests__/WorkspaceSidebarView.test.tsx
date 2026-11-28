// @vitest-environment jsdom

import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { cleanup, fireEvent, render, screen, within } from "@testing-library/react";

import { WorkspaceSidebar } from "../WorkspaceSidebar";
import { SessionActionProvider } from "../sidebar/SessionActionProvider";
import type { SessionResponse, Workspace } from "../../lib/types";
import { makeSession, makeWorkspace, sidebarProps, stubFetch } from "./fixtures";

type Props = React.ComponentProps<typeof WorkspaceSidebar>;

const ws = (id: string, over: Partial<SessionResponse> = {}): Workspace =>
  makeWorkspace(id, [makeSession({ id: `s-${id}`, title: id, project_path: "/repo-a", group_path: "", ...over })], {
    projectPath: "/repo-a",
  });

function renderSidebar(workspaces: Workspace[], over: Partial<Props> = {}) {
  const props = sidebarProps(workspaces, over);
  render(
    <SessionActionProvider
      sessions={workspaces.flatMap((w) => w.sessions)}
      runtimeEpoch="boot"
      observedById={{}}
      onSessionMutation={() => {}}
    >
      <WorkspaceSidebar {...props} />
    </SessionActionProvider>,
  );
  return props;
}

const toggle = () => screen.getByTestId("sidebar-view-toggle");
const openMenu = () => {
  fireEvent.click(toggle());
  return screen.getByRole("menu", { name: "View options" });
};
/** The filter row opens from the View options menu. */
const toggleFilter = () => {
  fireEvent.click(toggle());
  fireEvent.click(screen.getByTestId("sidebar-filter-toggle"));
};
const titles = () => screen.queryAllByTestId("sidebar-session-row").map((r) => r.textContent ?? "");

beforeEach(() => {
  stubFetch();
});
afterEach(() => {
  cleanup();
  vi.unstubAllGlobals();
});

describe("the sidebar view button", () => {
  it.each<[Partial<Props>, string, string]>([
    [{ groupBy: "project", sort: "custom" }, "project", "custom"],
    [{ groupBy: "group", sort: "name" }, "group", "name"],
    [{ groupBy: "project", sort: "newest" }, "project", "newest"],
  ])("marks the grouping and sort on the trigger %j", (over, groupBy, sort) => {
    renderSidebar([ws("a")], over);
    expect([toggle().getAttribute("data-group-by"), toggle().getAttribute("data-sort")]).toEqual([groupBy, sort]);
  });

  it("offers both groupings and the three sorts as radios and reports a pick", () => {
    const onGroupByChange = vi.fn();
    const onSortChange = vi.fn();
    renderSidebar([ws("a")], { groupBy: "project", sort: "custom", onGroupByChange, onSortChange });
    const menu = openMenu();
    const group = within(within(menu).getByRole("group", { name: "Group by" })).getAllByRole("menuitemradio");
    expect(group.map((r) => [r.textContent, r.getAttribute("aria-checked")])).toEqual([
      ["Repository", "true"],
      ["Groups", "false"],
    ]);
    const sort = within(within(menu).getByRole("group", { name: "Sort by" })).getAllByRole("menuitemradio");
    expect(sort.map((r) => [r.textContent, r.getAttribute("aria-checked")])).toEqual([
      ["Custom · drag to reorder", "true"],
      ["Newest first", "false"],
      ["Name", "false"],
    ]);
    // Focus starts on the checked grouping, for the arrow keys.
    expect(document.activeElement).toBe(group[0]);

    fireEvent.click(sort[1]!);
    expect(onSortChange).toHaveBeenCalledWith("newest");
    expect(screen.queryByRole("menu")).toBeNull();
    fireEvent.click(within(openMenu()).getByText("Groups"));
    expect(onGroupByChange).toHaveBeenCalledWith("group");
  });

  it.each<[string, Partial<Props>, string, boolean]>([
    ["read-only", { readOnly: true }, "This Daemon is read-only.", false],
    ["CityHall", { canArrangeView: false }, "Managed by CityHall.", true],
  ])("shows a viewer who cannot change the view the current choice and why: %s", (_, over, reason, canCreate) => {
    const onGroupByChange = vi.fn();
    renderSidebar([ws("a")], { ...over, groupBy: "project", onGroupByChange });
    const menu = openMenu();
    expect(menu.getAttribute("aria-describedby")).toBe(within(menu).getByText(reason).id);
    const radios = within(menu).getAllByRole("menuitemradio");
    for (const item of radios) expect(item.getAttribute("aria-disabled")).toBe("true");
    expect(radios.find((r) => r.getAttribute("aria-checked") === "true")?.textContent).toBe("Repository");
    fireEvent.click(within(menu).getByText("Groups"));
    expect(onGroupByChange).not.toHaveBeenCalled();
    const newGroup = within(menu).getByRole("menuitem", { name: "+ New group…" });
    // A read-only Daemon keeps it reachable, described by the reason, as the choices are.
    expect([(newGroup as HTMLButtonElement).disabled, newGroup.getAttribute("aria-disabled")]).toEqual([
      false,
      canCreate ? null : "true",
    ]);
    fireEvent.click(newGroup);
    expect(!!screen.queryByRole("dialog", { name: "New group" })).toBe(canCreate);
  });

  it.each(["project", "group"] as const)("ends the Group by list with New group under %s", (groupBy) => {
    renderSidebar([ws("a")], { groupBy });
    const items = Array.from(openMenu().querySelectorAll("[role^=menuitem]"));
    expect(items.map((i) => i.textContent)[2]).toBe("+ New group…");
  });
});

describe("arranging in CityHall", () => {
  it("leaves rows in place but lets Group headers move", () => {
    renderSidebar([ws("a", { group_path: "team" }), ws("b", { group_path: "team" })], { canArrangeView: false });
    expect(document.querySelectorAll("[aria-roledescription='Press and hold to reorder']")).toHaveLength(0);
    const headers = screen.getAllByTestId("sidebar-group-header");
    expect(headers.map((h) => h.getAttribute("data-draggable"))).toEqual(["true"]);
  });
});

describe("the Needs you filter", () => {
  it("shows only workspaces needing attention, under their headers, and hides the footer", () => {
    const workspaces = [
      ws("calm"),
      ws("waiting", { status: "Waiting" }),
      ws("broken", { status: "Error" }),
      ws("archived", { status: "Waiting", archived_at: "2026-01-01T00:00:00Z" }),
    ];
    renderSidebar(workspaces);
    expect(screen.getByTestId("sidebar-sunk-section")).toBeTruthy();
    toggleFilter();
    const needsYou = screen.getByTestId("sidebar-filter-needs-you");
    fireEvent.click(needsYou);
    expect(needsYou.getAttribute("aria-pressed")).toBe("true");
    expect(titles().map((t) => t.match(/waiting|broken|calm/)?.[0])).toEqual(["waiting", "broken"]);
    expect(screen.getAllByTestId("sidebar-group-header")).toHaveLength(1);
    expect(screen.queryByTestId("sidebar-sunk-section")).toBeNull();

    // Closing the filter clears it.
    toggleFilter();
    expect(titles()).toHaveLength(3);
  });

  it("says so when nothing needs you", () => {
    renderSidebar([ws("calm")]);
    toggleFilter();
    fireEvent.click(screen.getByTestId("sidebar-filter-needs-you"));
    expect(screen.getByText("Nothing needs you")).toBeTruthy();
  });
});
