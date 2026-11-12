// @vitest-environment jsdom

import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { cleanup, fireEvent, render, screen, within } from "@testing-library/react";

import { WorkspaceSidebar } from "../WorkspaceSidebar";
import { SessionActionProvider } from "../sidebar/SessionActionProvider";
import { buildSessionGroups } from "../../lib/sidebarGroups";
import type { SessionResponse, Workspace } from "../../lib/types";
import { makeSession, makeWorkspace, stubFetch } from "./fixtures";

type Props = React.ComponentProps<typeof WorkspaceSidebar>;
const noop = () => {};
const PAST = "2026-01-01T00:00:00Z";

/** One worktree whose sessions the group axis splits across two groups under the same workspace id. */
const split = (secondPinned: string | null): Workspace =>
  makeWorkspace(
    "w",
    (
      [
        { id: "s1", title: "one", group_path: "alpha" },
        { id: "s2", title: "two", group_path: "beta", pinned_at: secondPinned },
        { id: "s3", title: "three", group_path: "beta" },
      ] satisfies Partial<SessionResponse>[]
    ).map((s) => makeSession({ project_path: "/repo-a", branch: "feature/x", ...s })),
    { projectPath: "/repo-a", branch: "feature/x" },
  );

const sidebar = (ws: Workspace) => (
  <SessionActionProvider sessions={ws.sessions} runtimeEpoch="boot" observedById={{}} onSessionMutation={noop}>
    <WorkspaceSidebar
      {...({
        observedById: {},
        runtimeEpoch: "boot",
        onSessionMutation: noop,
        groups: buildSessionGroups([ws], {
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
        activeSessionId: null,
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
        sortMode: "lastActivity",
        onSortModeChange: noop,
        pluginSortRef: null,
        onPluginSortChange: noop,
        axis: "group",
        onAxisChange: noop,
      } satisfies Props)}
    />
  </SessionActionProvider>
);

/** One group (no `group_path`), so a workspace's sessions stay together and the row expands at two or more. */
const members = (ids: string[], firstPinned: string | null = null): Workspace =>
  makeWorkspace(
    "w",
    ids.map((id, i) =>
      makeSession({
        id,
        title: id,
        project_path: "/repo-a",
        branch: "feature/x",
        group_path: "",
        pinned_at: i === 0 ? firstPinned : null,
      }),
    ),
    { projectPath: "/repo-a", branch: "feature/x" },
  );

const firstExpandedRow = () =>
  within(screen.getByTestId("sidebar-expanded-sessions")).getAllByTestId("sidebar-session-row")[0]!;
const pinMark = (row: HTMLElement) => within(row).queryByTestId("sidebar-pinned-mark");
const pinFirstExpandedRow = async () => {
  fireEvent.contextMenu(firstExpandedRow());
  fireEvent.click(screen.getByTestId("sidebar-context-menu-pin"));
  await vi.waitFor(() => expect(pinMark(firstExpandedRow())).not.toBeNull());
};

beforeEach(() => {
  stubFetch();
});
afterEach(() => {
  cleanup();
  vi.unstubAllGlobals();
});

describe("expanded rows when the group axis splits one workspace across groups", () => {
  it("reconciles a pin on a later slice's child, so a later canonical unpin is not masked", async () => {
    const { rerender } = render(sidebar(split(null)));
    // Only the beta slice ([s2, s3]) is expandable; alpha holds s1 alone.
    fireEvent.click(screen.getByTestId("sidebar-group-expand-toggle"));
    expect(pinMark(firstExpandedRow())).toBeNull();

    await pinFirstExpandedRow();

    // The server acknowledges the pin, then someone unpins it elsewhere.
    rerender(sidebar(split(PAST)));
    expect(pinMark(firstExpandedRow())).not.toBeNull();
    rerender(sidebar(split(null)));
    expect(pinMark(firstExpandedRow())).toBeNull();
  });
});

describe("expanded rows when the worktree's membership changes", () => {
  it("keeps reconciling the surviving session while it is alone, so a rejoining sibling does not unmask a stale pin", async () => {
    const { rerender } = render(sidebar(members(["a", "b"])));
    fireEvent.click(screen.getByTestId("sidebar-group-expand-toggle"));
    await pinFirstExpandedRow();

    // The sibling goes away before the pin is acknowledged: 2 -> 1.
    rerender(sidebar(members(["a"])));
    expect(screen.queryByTestId("sidebar-expanded-sessions")).toBeNull();
    // While alone, the pin is acknowledged and then undone elsewhere.
    rerender(sidebar(members(["a"], PAST)));
    rerender(sidebar(members(["a"])));

    // A second session joins: 1 -> 2. The row is still expanded and must show the canonical unpinned state.
    rerender(sidebar(members(["a", "c"])));
    expect(pinMark(firstExpandedRow())).toBeNull();
  });
});
