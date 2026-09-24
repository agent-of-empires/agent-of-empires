// @vitest-environment jsdom

import { afterEach, describe, expect, it, vi } from "vitest";
import { cleanup, fireEvent, render, screen } from "@testing-library/react";

import { WorkspaceSidebar } from "../WorkspaceSidebar";
import { buildSessionGroups } from "../../lib/sidebarGroups";
import type { SessionResponse, Workspace } from "../../lib/types";
import { makeSession, makeWorkspace } from "./fixtures";

type Props = React.ComponentProps<typeof WorkspaceSidebar>;
const noop = () => {};
const TRASHED = "2026-01-01T00:00:00Z";

const workspace = (id: string, sessions: Partial<SessionResponse>[]) =>
  makeWorkspace(
    id,
    sessions.map((s) => makeSession({ title: "t", project_path: "/repo-a", group_path: "", status: "Stopped", ...s })),
    { projectPath: "/repo-a" },
  );
const trashed = (id: string, ...ids: string[]) =>
  workspace(
    id,
    ids.map((sid) => ({ id: sid, trashed_at: TRASHED })),
  );

/** `groups` holds the workspaces for navigation; the Trash list is passed separately, as App computes it. */
function renderSidebar(workspaces: Workspace[], over: Partial<Props> = {}) {
  const props: Props = {
    groups: buildSessionGroups(workspaces, {
      idleDecayWindowMs: 60_000,
      sortMode: "lastActivity",
      isCollapsed: () => false,
    }),
    nestedGroups: [],
    orgGroups: [],
    onToggleSubgroup: noop,
    onToggleOrg: noop,
    onToggleOrgRepo: noop,
    onReorderWorkspaces: noop,
    onReorderGroups: noop,
    activeId: null,
    open: true,
    onToggle: noop,
    onSelect: vi.fn(),
    onToggleGroup: noop,
    onUpdateRepoAppearance: noop,
    onNew: noop,
    onCreateSession: noop,
    savedProjects: [],
    onAddProject: noop,
    onEditProject: noop,
    onRemoveProject: noop,
    onSettings: noop,
    onRestoreSession: vi.fn(),
    onDeleteSession: vi.fn(),
    onEmptyTrash: vi.fn(),
    sortMode: "lastActivity",
    onSortModeChange: noop,
    pluginSortRef: null,
    onPluginSortChange: noop,
    axis: "group",
    onAxisChange: noop,
    ...over,
  };
  render(<WorkspaceSidebar {...props} />);
  return props;
}
const withTrash = (over: Partial<Props> = {}) => {
  const ws = trashed("trashed-ws", "s1");
  return renderSidebar([ws], { trashedWorkspaces: [ws], ...over });
};
const click = (id: string) => fireEvent.click(screen.getByTestId(id));
const query = (id: string) => screen.queryByTestId(id);
const follows = (a: Node, b: Node) => !!(a.compareDocumentPosition(b) & Node.DOCUMENT_POSITION_FOLLOWING);

afterEach(cleanup);

describe("WorkspaceSidebar Trash control", () => {
  it("opens from the footer and exposes Open, Restore, and Delete", () => {
    const props = withTrash();
    expect(query("sidebar-trash-menu")).toBeNull();
    click("sidebar-trash-toggle");
    expect(query("sidebar-trash-row")).not.toBeNull();

    click("sidebar-trash-open");
    expect(props.onSelect).toHaveBeenCalledWith("trashed-ws", "s1");
    expect(query("sidebar-trash-menu")).toBeNull();

    click("sidebar-trash-toggle");
    click("sidebar-trash-restore");
    expect(props.onRestoreSession).toHaveBeenCalledWith(["s1"]);
    click("sidebar-trash-purge");
    expect(props.onDeleteSession).toHaveBeenCalledWith(["s1"]);
  });

  it("orders rows newest-trashed first", () => {
    const older = workspace("older-ws", [{ id: "o1", trashed_at: TRASHED }]);
    const newer = workspace("newer-ws", [{ id: "n1", trashed_at: "2026-06-01T00:00:00Z" }]);
    renderSidebar([older, newer], { trashedWorkspaces: [older, newer] });
    click("sidebar-trash-toggle");
    expect(screen.getAllByTestId("sidebar-trash-row").map((r) => r.textContent?.slice(0, 8))).toEqual([
      "newer-ws",
      "older-ws",
    ]);
  });

  it("puts the count right after the Trash icon, not against Settings (#2574)", () => {
    withTrash();
    const toggle = screen.getByTestId("sidebar-trash-toggle");
    const badge = screen.getByTestId("sidebar-trash-count");
    expect(badge.textContent).toBe("1");
    expect(toggle.contains(badge)).toBe(true);
    const label = Array.from(toggle.querySelectorAll("span")).find((s) => s.textContent === "Trash")!;
    expect(follows(badge, label)).toBe(true);
  });

  it.each([
    ["Escape", () => fireEvent.keyDown(document, { key: "Escape" })],
    ["an outside click", () => fireEvent.mouseDown(document.body)],
  ])("closes on %s", (_n, dismiss) => {
    withTrash();
    click("sidebar-trash-toggle");
    dismiss();
    expect(query("sidebar-trash-menu")).toBeNull();
  });

  it("stays reachable while a filter hides every live row (#2512)", () => {
    withTrash();
    fireEvent.click(screen.getByLabelText("Filter sessions"));
    fireEvent.change(screen.getByTestId("sidebar-filter-input"), { target: { value: "zzz-no-match" } });
    click("sidebar-trash-toggle");
    expect(query("sidebar-trash-row")).not.toBeNull();
  });

  it("hides Restore, Delete, and Empty Trash when read-only", () => {
    withTrash({ readOnly: true });
    click("sidebar-trash-toggle");
    expect(query("sidebar-trash-open")).not.toBeNull();
    for (const id of ["restore", "purge", "empty"]) expect(query(`sidebar-trash-${id}`)).toBeNull();
  });

  it.each([
    [["c1"], "Permanently delete 1 trashed session? This cannot be undone."],
    [["c1", "c2"], "Permanently delete 2 trashed sessions? This cannot be undone."],
  ])("Empty Trash confirm counts sessions %j (#3167)", (ids, expected) => {
    const ws = trashed("multi-ws", ...ids);
    renderSidebar([ws], { trashedWorkspaces: [ws] });
    click("sidebar-trash-toggle");
    click("sidebar-trash-empty");
    expect(screen.getByTestId("empty-trash-dialog").textContent).toContain(expected);
  });

  it("Empty Trash: Cancel is inert, Escape keeps the panel open, one Confirm purges once", () => {
    const props = withTrash();
    click("sidebar-trash-toggle");
    click("sidebar-trash-empty");
    fireEvent.click(screen.getByRole("button", { name: "Cancel" }));
    expect(query("empty-trash-dialog")).toBeNull();

    // The confirm portals outside the panel, so Escape must not close the panel too.
    click("sidebar-trash-empty");
    fireEvent.keyDown(document, { key: "Escape" });
    expect(query("empty-trash-dialog")).toBeNull();
    expect(query("sidebar-trash-menu")).not.toBeNull();
    expect(props.onEmptyTrash).not.toHaveBeenCalled();

    click("sidebar-trash-empty");
    click("empty-trash-confirm");
    expect(props.onEmptyTrash).toHaveBeenCalledTimes(1);
  });

  it("omits the control when nothing is trashed, and lists Projects below Snoozed & archived", () => {
    renderSidebar([workspace("archived-ws", [{ id: "a1", archived_at: TRASHED }])]);
    expect(query("sidebar-trash-toggle")).toBeNull();
    expect(follows(screen.getByTestId("sidebar-sunk-section"), screen.getByTestId("sidebar-projects-section"))).toBe(
      true,
    );
  });
});

describe("WorkspaceSidebar row actions on a group slice (#4019)", () => {
  it("stop, start, and delete target only the row's sessions, not the whole workspace", () => {
    const ws = workspace("multi-ws", [
      { id: "a1", title: "alpha", group_path: "alpha" },
      { id: "b1", title: "beta", group_path: "beta", status: "Running" },
      { id: "b2", title: "beta", group_path: "beta", status: "Running" },
    ]);
    const onStopSession = vi.fn();
    const onStartSession = vi.fn();
    const props = renderSidebar([ws], { onStopSession, onStartSession });
    const act = (title: string, item: string) => {
      fireEvent.contextMenu(screen.getAllByTestId("sidebar-session-row").find((r) => r.textContent?.includes(title))!);
      click(`sidebar-context-menu-${item}`);
    };

    act("beta", "stop");
    expect(onStopSession).toHaveBeenCalledWith("b1");
    act("alpha", "start");
    expect(onStartSession).toHaveBeenCalledWith("a1");
    act("beta", "delete");
    expect(props.onDeleteSession).toHaveBeenCalledWith(["b1", "b2"]);
  });
});
