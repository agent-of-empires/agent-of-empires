// @vitest-environment jsdom

import { afterEach, describe, expect, it, vi } from "vitest";
import { cleanup, fireEvent, render } from "@testing-library/react";

import { TopBar } from "../TopBar";
import type { SessionResponse, Workspace } from "../../lib/types";
import type { AttentionBadgeColors } from "../../lib/attentionBadgeColors";

const DEFAULT_ATTENTION_BADGE_COLORS: AttentionBadgeColors = {
  unreadBg: "#38bdf8",
  unreadFg: "#000000",
  waitingBg: "#fbbf24",
  waitingFg: "#000000",
};

afterEach(() => {
  cleanup();
});

type TopBarOverrides = {
  isDevBuild?: boolean;
  isOffline?: boolean;
  activeWorkspace?: Workspace;
  activeSession?: SessionResponse | null;
  onOpenTips?: () => void;
  onToggleSidebar?: () => void;
  unreadCount?: number;
  waitingCount?: number;
  attentionBadgeColors?: AttentionBadgeColors;
};

function topBarProps(overrides: TopBarOverrides = {}) {
  return {
    activeWorkspace: overrides.activeWorkspace,
    activeSession: overrides.activeSession ?? null,
    onToggleSidebar: overrides.onToggleSidebar ?? vi.fn(),
    onOpenPalette: vi.fn(),
    onToggleDiff: vi.fn(),
    paneIds: ["diff", "terminal"],
    paneDescriptor: (id: string) => ({ title: id, icon: (() => null) as never }),
    isPaneOpen: () => true,
    onTogglePane: vi.fn(),
    onOpenHelp: vi.fn(),
    onOpenAbout: vi.fn(),
    onStartTutorial: vi.fn(),
    unreadCount: overrides.unreadCount ?? 0,
    waitingCount: overrides.waitingCount ?? 0,
    attentionBadgeColors: overrides.attentionBadgeColors ?? DEFAULT_ATTENTION_BADGE_COLORS,
    onLogout: vi.fn(),
    loginRequired: false,
    isOffline: overrides.isOffline ?? false,
    isDevBuild: overrides.isDevBuild ?? false,
    onOpenTips: overrides.onOpenTips ?? vi.fn(),
    onGoDashboard: vi.fn(),
    sidebarColumnVisible: false,
    rightColumnVisible: false,
  };
}

function renderTopBar(overrides: TopBarOverrides = {}) {
  return render(<TopBar {...topBarProps(overrides)} />);
}

describe("TopBar", () => {
  it("renders the DEV and offline badges independently", () => {
    const off = renderTopBar({ isDevBuild: false });
    expect(off.queryByLabelText("Debug build")).toBeNull();
    expect(off.queryByText("DEV")).toBeNull();
    off.unmount();
    const { getByText, getByLabelText } = renderTopBar({ isDevBuild: true, isOffline: true });
    expect(getByText("DEV")).toBeTruthy();
    expect(getByLabelText("Debug build")).toBeTruthy();
    expect(getByText("offline")).toBeTruthy();
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

  it("exposes a Tips entry in the overflow menu that fires onOpenTips", () => {
    const onOpenTips = vi.fn();
    const { getByRole } = renderTopBar({ onOpenTips });
    fireEvent.click(getByRole("button", { name: "More options" }));
    fireEvent.click(getByRole("menuitem", { name: "Tips" }));
    expect(onOpenTips).toHaveBeenCalledTimes(1);
  });

  it("shows counts on the sidebar toggle's badges and accessible name, and nothing extra at zero", () => {
    const zero = renderTopBar({ unreadCount: 0, waitingCount: 0 });
    expect(zero.queryByTestId("topbar-unread-badge")).toBeNull();
    expect(zero.queryByTestId("topbar-waiting-badge")).toBeNull();
    expect(zero.getByRole("button", { name: "Toggle sidebar" })).toBeTruthy();
    zero.unmount();

    const onToggleSidebar = vi.fn();
    const { getByTestId, getByRole } = renderTopBar({ unreadCount: 3, waitingCount: 1, onToggleSidebar });
    expect(getByTestId("topbar-unread-badge").textContent).toBe("3");
    expect(getByTestId("topbar-waiting-badge").textContent).toBe("1");
    expect(getByRole("button", { name: "Toggle sidebar, 3 unread, 1 waiting for your input" })).toBeTruthy();
    // Badges nest inside the toggle, so tapping one toggles the sidebar.
    fireEvent.click(getByTestId("topbar-unread-badge"));
    expect(onToggleSidebar).toHaveBeenCalledTimes(1);
  });

  it("colors each badge from the attentionBadgeColors prop", () => {
    const { getByTestId } = renderTopBar({
      unreadCount: 3,
      waitingCount: 1,
      attentionBadgeColors: { unreadBg: "#0f0f11", unreadFg: "#ffffff", waitingBg: "#fe640b", waitingFg: "#000000" },
    });
    const unreadBadge = getByTestId("topbar-unread-badge");
    const waitingBadge = getByTestId("topbar-waiting-badge");
    expect(unreadBadge.style.backgroundColor).toBe("rgb(15, 15, 17)");
    expect(unreadBadge.style.color).toBe("rgb(255, 255, 255)");
    expect(waitingBadge.style.backgroundColor).toBe("rgb(254, 100, 11)");
    expect(waitingBadge.style.color).toBe("rgb(0, 0, 0)");
  });

  it("announces count changes through a persistent live region, including the return to zero", () => {
    const { getByTestId, rerender } = renderTopBar({ unreadCount: 0, waitingCount: 0 });
    const live = getByTestId("topbar-attention-live-region");
    expect(live.getAttribute("aria-live")).toBe("polite");
    expect(live.textContent).toBe("0 unread, 0 waiting for your input");

    rerender(<TopBar {...topBarProps({ unreadCount: 2, waitingCount: 0 })} />);
    expect(getByTestId("topbar-attention-live-region").textContent).toBe("2 unread, 0 waiting for your input");

    // Clearing the last attention item is a real text mutation (not an empty string), so screen readers get a
    // reliable announcement here too.
    rerender(<TopBar {...topBarProps({ unreadCount: 0, waitingCount: 0 })} />);
    expect(getByTestId("topbar-attention-live-region").textContent).toBe("0 unread, 0 waiting for your input");
  });
});
