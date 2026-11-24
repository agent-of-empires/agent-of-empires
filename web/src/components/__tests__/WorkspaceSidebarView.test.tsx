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
  return screen.getByRole("menu", { name: "Sidebar view" });
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
  it.each<[Partial<Props>, string]>([
    [{ groupBy: "project", sort: "custom" }, "Repository · Custom"],
    [{ groupBy: "group", sort: "name" }, "My groups · Name"],
  ])("reads the grouping and sort %j", (over, label) => {
    renderSidebar([ws("a")], over);
    expect(toggle().getAttribute("aria-label")).toBe(`Sidebar view: ${label}`);
    expect(toggle().textContent?.replace("·", " · ").replace(/\s+/g, " ")).toBe(label);
  });

  it("offers both groupings and both sorts as radios and reports a pick", () => {
    const onGroupByChange = vi.fn();
    const onSortChange = vi.fn();
    renderSidebar([ws("a")], { groupBy: "project", sort: "custom", onGroupByChange, onSortChange });
    const menu = openMenu();
    const group = within(within(menu).getByRole("group", { name: "Group by" })).getAllByRole("menuitemradio");
    expect(group.map((r) => [r.textContent, r.getAttribute("aria-checked")])).toEqual([
      ["Repository", "true"],
      ["My groups", "false"],
    ]);
    const sort = within(within(menu).getByRole("group", { name: "Sort" })).getAllByRole("menuitemradio");
    expect(sort.map((r) => [r.textContent, r.getAttribute("aria-checked")])).toEqual([
      ["Customdrag to reorder", "true"],
      ["Name", "false"],
    ]);
    // Focus starts on the checked grouping, for the arrow keys.
    expect(document.activeElement).toBe(group[0]);

    fireEvent.click(sort[1]!);
    expect(onSortChange).toHaveBeenCalledWith("name");
    expect(screen.queryByRole("menu")).toBeNull();
    fireEvent.click(within(openMenu()).getByText("My groups"));
    expect(onGroupByChange).toHaveBeenCalledWith("group");
  });

  it("resets the Custom order from the sort section", () => {
    const onResetCustomOrder = vi.fn();
    renderSidebar([ws("a")], { onResetCustomOrder });
    fireEvent.click(within(openMenu()).getByRole("menuitem", { name: "Reset to newest first" }));
    expect(onResetCustomOrder).toHaveBeenCalledTimes(1);
  });

  it("leaves the shared choices to a viewer who may change them", () => {
    renderSidebar([ws("a")], { readOnly: true });
    const menu = openMenu();
    for (const item of within(menu).getAllByRole("menuitemradio"))
      expect((item as HTMLButtonElement).disabled).toBe(true);
    const reset = within(menu).getByRole("menuitem", { name: "Reset to newest first" }) as HTMLButtonElement;
    expect(reset.disabled).toBe(true);
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
    fireEvent.click(screen.getByRole("button", { name: "Filter sessions" }));
    const needsYou = screen.getByTestId("sidebar-filter-needs-you");
    fireEvent.click(needsYou);
    expect(needsYou.getAttribute("aria-pressed")).toBe("true");
    expect(titles().map((t) => t.match(/waiting|broken|calm/)?.[0])).toEqual(["waiting", "broken"]);
    expect(screen.getAllByTestId("sidebar-group-header")).toHaveLength(1);
    expect(screen.queryByTestId("sidebar-sunk-section")).toBeNull();

    // Closing the filter clears it.
    fireEvent.click(screen.getByRole("button", { name: "Filter sessions" }));
    expect(titles()).toHaveLength(3);
  });

  it("says so when nothing needs you", () => {
    renderSidebar([ws("calm")]);
    fireEvent.click(screen.getByRole("button", { name: "Filter sessions" }));
    fireEvent.click(screen.getByTestId("sidebar-filter-needs-you"));
    expect(screen.getByText("Nothing needs you")).toBeTruthy();
  });
});
