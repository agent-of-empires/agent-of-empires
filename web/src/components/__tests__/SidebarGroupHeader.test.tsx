// @vitest-environment jsdom
// Drag-release click suppression is timing dependent in a browser, so it is pinned here via `isDragging`.

import { afterEach, describe, expect, it, vi } from "vitest";
import { cleanup, fireEvent, render, screen } from "@testing-library/react";

import { SidebarGroupHeader } from "../sidebar/SidebarGroupHeader";
import type { SidebarGroup } from "../../lib/sidebarGroups";
import type { SessionStatus, Workspace } from "../../lib/types";
import { makeSession, makeWorkspace } from "./fixtures";

type Sunk = "archived" | "snoozed";
const SUNK = { archived: { archived_at: "2025-01-02T00:00:00Z" }, snoozed: { snoozed_until: "2099-01-01T00:00:00Z" } };

function workspace(id: string, statuses: SessionStatus[], sunk?: Sunk): Workspace {
  return makeWorkspace(
    id,
    statuses.map((status, i) => makeSession({ id: `${id}-s${i}`, title: id, status, ...(sunk && SUNK[sunk]) })),
  );
}
const idle = (n: number) => Array<SessionStatus>(n).fill("Idle");
const views = (...wss: Workspace[]) => wss.map((w) => ({ key: w.id, workspace: w }));

function group(over: Partial<SidebarGroup> = {}): SidebarGroup {
  return {
    id: "g1",
    kind: "repo",
    displayName: "my-project",
    defaultDisplayName: "my-project",
    alias: null,
    color: null,
    remoteOwner: null,
    workspaces: views(workspace("w1", idle(3))),
    status: "idle",
    collapsed: false,
    capabilities: { appearance: true, reorder: true, create: "repo" },
    repoPath: "/p",
    registeredProjects: [],
    pinned: false,
    pinnedEmpty: false,
    ...over,
  };
}

const dragHandle = (isDragging: boolean) => ({
  setActivatorNodeRef: () => {},
  attributes: {},
  listeners: {},
  isDragging,
});

function renderHeader(props: Partial<Parameters<typeof SidebarGroupHeader>[0]> = {}) {
  const onClick = vi.fn();
  const onNewSession = vi.fn();
  const ui = (p: typeof props) => (
    <SidebarGroupHeader
      group={group()}
      hasActiveChild={false}
      onClick={onClick}
      onNewSession={onNewSession}
      onUpdateAppearance={() => {}}
      offline={false}
      {...p}
    />
  );
  const { rerender } = render(ui(props));
  return { onClick, onNewSession, rerender: (p: typeof props) => rerender(ui(p)) };
}

const text = (id: string) => screen.queryByTestId(id)?.textContent;

afterEach(cleanup);

describe("SidebarGroupHeader", () => {
  it.each([
    ["two live workspaces", views(workspace("a", idle(2)), workspace("b", idle(3))), "(2)"],
    // Sunk rows render in the footer, so they are not counted (#2372).
    [
      "live plus sunk",
      views(workspace("live", idle(1)), workspace("arch", idle(1), "archived"), workspace("zz", idle(1), "snoozed")),
      "(1)",
    ],
  ])("counts %s as %s", (_n, workspaces, count) => {
    renderHeader({ group: group({ workspaces }) });
    expect(text("sidebar-group-session-count")).toBe(count);
  });

  it.each([
    ["Waiting and Error sessions", views(workspace("a", ["Waiting", "Running"]), workspace("b", ["Error"])), "2"],
    ["nothing needing attention", views(workspace("w1", idle(3))), undefined],
    // Sunk wins over Waiting.
    ["waiting but archived sessions", views(workspace("arch", ["Waiting", "Waiting"], "archived")), undefined],
  ])("attention badge for %s is %s", (_n, workspaces, badge) => {
    renderHeader({ group: group({ workspaces }) });
    expect(text("sidebar-group-attention-badge")).toBe(badge);
  });

  it("renders the owner avatar when there is a remote owner, else the folder icon", () => {
    renderHeader({ group: group({ remoteOwner: "octocat" }) });
    expect(screen.getByRole("img").getAttribute("alt")).toBe("octocat");
    cleanup();
    renderHeader();
    expect(screen.queryByRole("img")).toBeNull();
    expect(screen.getByTestId("sidebar-group-icon")).not.toBeNull();
  });

  it("marks the header draggable only with a drag handle", () => {
    const { rerender } = renderHeader({ dragHandle: dragHandle(false) });
    expect(screen.getByTestId("sidebar-group-header").getAttribute("data-draggable")).toBe("true");
    rerender({});
    expect(screen.getByTestId("sidebar-group-header").getAttribute("data-draggable")).toBeNull();
  });

  it("swallows clicks on every control during a drag and briefly after, then toggles again", () => {
    vi.useFakeTimers();
    try {
      const { onClick, onNewSession, rerender } = renderHeader({ dragHandle: dragHandle(true) });
      fireEvent.click(screen.getByText("my-project"));
      fireEvent.click(screen.getByLabelText("New session in my-project"));
      rerender({ dragHandle: dragHandle(false) });
      fireEvent.click(screen.getByText("my-project"));
      expect(onClick).not.toHaveBeenCalled();
      expect(onNewSession).not.toHaveBeenCalled();

      vi.advanceTimersByTime(300);
      fireEvent.click(screen.getByText("my-project"));
      expect(onClick).toHaveBeenCalledTimes(1);
    } finally {
      vi.useRealTimers();
    }
  });
});
