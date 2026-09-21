// @vitest-environment jsdom

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
