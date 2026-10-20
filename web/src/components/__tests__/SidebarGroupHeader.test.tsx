// @vitest-environment jsdom
// Drag-release click suppression is timing dependent in a browser, so it is pinned here via `isDragging`.

import { afterEach, describe, expect, it, vi } from "vitest";
import { cleanup, fireEvent, render, screen } from "@testing-library/react";

import { SidebarGroupHeader } from "../sidebar/SidebarGroupHeader";
import type { SidebarGroup } from "../../lib/sidebarGroups";
import { PluginUiProvider } from "../../lib/pluginUiContext";
import type { PluginUiEntry } from "../../lib/api";
import type { SessionStatus, Workspace } from "../../lib/types";
import { makeSession, makeWorkspace } from "./fixtures";

const { entriesRef } = vi.hoisted(() => ({ entriesRef: { current: [] as PluginUiEntry[] } }));
vi.mock("../../hooks/usePluginUiState", () => ({
  usePluginUiState: () => ({ entries: entriesRef.current, revisions: {}, isRefreshing: false, poke: () => {} }),
}));

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
    remoteOwnerKey: null,
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

const dragHandle = () => ({
  setActivatorNodeRef: () => {},
  attributes: {},
  listeners: {},
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

  it.each([
    ["danger", "1"],
    ["warn", "1"],
    ["info", undefined],
    ["neutral", undefined],
  ])("attention badge for a %s plugin row-column on an idle session is %s (#4446)", (tone, badge) => {
    entriesRef.current = [
      { plugin_id: "gh", slot: "row-column", id: "pr", session_id: "w1-s0", payload: { text: "PR", tone } },
    ];
    try {
      render(
        <PluginUiProvider>
          <SidebarGroupHeader
            group={group({ workspaces: views(workspace("w1", idle(2))) })}
            hasActiveChild={false}
            onClick={() => {}}
            onNewSession={() => {}}
            onUpdateAppearance={() => {}}
            offline={false}
          />
        </PluginUiProvider>,
      );
      expect(text("sidebar-group-attention-badge")).toBe(badge);
    } finally {
      entriesRef.current = [];
    }
  });

  it("shows the owner avatar only for GitHub remotes, and the folder otherwise or once the image fails", () => {
    const folder = () => screen.getByTestId("sidebar-group-icon").querySelector("svg.lucide-folder");
    // (remoteOwnerKey, avatar src); no src means no image, so no request.
    const cases: Array<[string | null, string | null]> = [
      ["octocat@github.com", "https://github.com/octocat.png?size=32"],
      ["acme@gitlab.com", null],
      [null, null],
    ];
    for (const [key, src] of cases) {
      renderHeader({ group: group({ remoteOwnerKey: key }) });
      const img = screen.queryByRole("img");
      expect(img?.getAttribute("src") ?? null, String(key)).toBe(src);
      if (img) {
        expect(img.getAttribute("alt")).toBe("octocat");
        expect(folder()).toBeNull();
        fireEvent.error(img);
        expect(screen.queryByRole("img")).toBeNull();
      }
      expect(folder(), String(key)).not.toBeNull();
      cleanup();
    }
  });

  it("marks the header draggable only with a drag handle", () => {
    const { rerender } = renderHeader({ dragHandle: dragHandle() });
    expect(screen.getByTestId("sidebar-group-header").getAttribute("data-draggable")).toBe("true");
    rerender({});
    expect(screen.getByTestId("sidebar-group-header").getAttribute("data-draggable")).toBeNull();
  });

  it("offers 'Edit settings' for the synthetic Scratch group despite capabilities.create being 'generic'", () => {
    const onEditProjectSettings = vi.fn();
    renderHeader({
      group: group({ id: "__scratch__", capabilities: { appearance: true, reorder: true, create: "generic" } }),
      onEditProject: onEditProjectSettings,
    });

    fireEvent.contextMenu(screen.getByTestId("sidebar-group-header"));
    fireEvent.click(screen.getByTestId("sidebar-group-context-menu-settings"));
    expect(onEditProjectSettings).toHaveBeenCalledWith(expect.objectContaining({ id: "__scratch__" }));
  });

  it("hides 'Edit settings' for an ordinary unregistered repo group ('generic' create, no repoPath match)", () => {
    renderHeader({
      group: group({ id: "other", capabilities: { appearance: true, reorder: true, create: "generic" } }),
      onEditProject: vi.fn(),
    });

    fireEvent.contextMenu(screen.getByTestId("sidebar-group-header"));
    expect(screen.queryByTestId("sidebar-group-context-menu-settings")).toBeNull();
  });
});
