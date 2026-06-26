// @vitest-environment jsdom
//
// Presentational contract test for TopBar. TopBar is a pure prop-driven
// component (it pulls no data on its own), so this suite renders it
// directly with the prop permutations we care about and asserts the
// surface badges/buttons match. The full mounted topbar is exercised
// end-to-end in web/tests/top-bar.spec.ts; that suite covers menu
// interaction but cannot exercise the dev-build badge without mocking
// `/api/about`, which is what this Vitest file does instead.
//
// Part of #1055 (DEV build badge so concurrently-running debug/release
// instances on ports 8081 / 8080 are visually distinguishable).

import { afterEach, describe, expect, it, vi } from "vitest";
import { cleanup, fireEvent, render } from "@testing-library/react";

import { TopBar } from "../TopBar";
import type { SessionResponse, Workspace } from "../../lib/types";

afterEach(() => {
  cleanup();
});

function renderTopBar(
  overrides: {
    isDevBuild?: boolean;
    isOffline?: boolean;
    activeWorkspace?: Workspace;
    activeSession?: SessionResponse | null;
    onOpenTips?: () => void;
  } = {},
) {
  return render(
    <TopBar
      activeWorkspace={overrides.activeWorkspace}
      activeSession={overrides.activeSession ?? null}
      onToggleSidebar={vi.fn()}
      onOpenPalette={vi.fn()}
      onToggleDiff={vi.fn()}
      paneIds={["diff", "terminal"]}
      paneDescriptor={(id) => ({ title: id, icon: (() => null) as never })}
      isPaneOpen={() => true}
      onTogglePane={vi.fn()}
      onOpenHelp={vi.fn()}
      onOpenAbout={vi.fn()}
      onStartTutorial={vi.fn()}
      onLogout={vi.fn()}
      loginRequired={false}
      isOffline={overrides.isOffline ?? false}
      isDevBuild={overrides.isDevBuild ?? false}
      onOpenTips={overrides.onOpenTips ?? vi.fn()}
      onGoDashboard={vi.fn()}
      sidebarColumnVisible={false}
      rightColumnVisible={false}
    />,
  );
}

describe("TopBar", () => {
  it("renders the DEV badge when isDevBuild=true", () => {
    const { getByLabelText, getByText } = renderTopBar({ isDevBuild: true });
    const badge = getByLabelText("Debug build");
    expect(badge).toBeTruthy();
    expect(getByText("DEV")).toBeTruthy();
  });

  it("does not render the DEV badge when isDevBuild=false", () => {
    const { queryByLabelText, queryByText } = renderTopBar({
      isDevBuild: false,
    });
    expect(queryByLabelText("Debug build")).toBeNull();
    expect(queryByText("DEV")).toBeNull();
  });

  it("does not render the workspace/repo breadcrumb even with an active workspace and session", () => {
    const workspace = {
      id: "ws-1",
      branch: null,
      projectPath: "/home/user/breadcrumb-repo",
      displayName: "breadcrumb-feature",
      agents: [],
      primaryAgent: "claude",
      status: "idle",
      sessions: [],
    } as unknown as Workspace;
    const { queryByText } = renderTopBar({
      activeWorkspace: workspace,
      activeSession: {} as SessionResponse,
    });
    // The old breadcrumb rendered the repo name (last path segment) and the
    // workspace display name; both must be gone now that #1456 removed it.
    expect(queryByText("breadcrumb-repo")).toBeNull();
    expect(queryByText("breadcrumb-feature")).toBeNull();
  });

  it("renders the offline badge independent of the DEV badge", () => {
    const { getByText, getByLabelText } = renderTopBar({
      isDevBuild: true,
      isOffline: true,
    });
    expect(getByText("offline")).toBeTruthy();
    expect(getByLabelText("Debug build")).toBeTruthy();
  });

  it("exposes a Tips entry in the overflow menu that fires onOpenTips", () => {
    const onOpenTips = vi.fn();
    const { getByRole } = renderTopBar({ onOpenTips });
    fireEvent.click(getByRole("button", { name: "More options" }));
    fireEvent.click(getByRole("menuitem", { name: "Tips" }));
    expect(onOpenTips).toHaveBeenCalledTimes(1);
  });
});
