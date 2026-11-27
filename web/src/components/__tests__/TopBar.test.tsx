// @vitest-environment jsdom

import { afterEach, describe, expect, it, vi } from "vitest";
import { act, cleanup, fireEvent, render } from "@testing-library/react";
import { MemoryRouter } from "react-router-dom";

import { useSessions } from "../../hooks/useSessions";
import { setServerDown } from "../../lib/connectionState";
import { TopBar } from "../TopBar";
import { DisconnectBanner } from "../DisconnectBanner";
import { useConnectionBanner } from "../../hooks/useConnectionBanner";
import type { SessionResponse, Workspace } from "../../lib/types";

afterEach(() => {
  cleanup();
  vi.clearAllTimers();
  vi.useRealTimers();
  vi.unstubAllGlobals();
  setServerDown(false);
});

type TopBarOverrides = {
  isDevBuild?: boolean;
  activeWorkspace?: Workspace;
  activeSession?: SessionResponse | null;
  onToggleSidebar?: () => void;
  needsYouCount?: number;
  sidebarColumnVisible?: boolean;
};

function topBarProps(overrides: TopBarOverrides = {}) {
  return {
    activeWorkspace: overrides.activeWorkspace,
    activeSession: overrides.activeSession ?? null,
    onToggleSidebar: overrides.onToggleSidebar ?? vi.fn(),
    onOpenPalette: vi.fn(),
    paneIds: ["diff", "terminal"],
    paneDescriptor: (id: string) => ({ title: id, icon: (() => null) as never }),
    isPaneOpen: () => true,
    onTogglePane: vi.fn(),
    needsYouCount: overrides.needsYouCount ?? 0,
    isDevBuild: overrides.isDevBuild ?? false,
    appMenu: [],
    onGoDashboard: vi.fn(),
    sidebarColumnVisible: overrides.sidebarColumnVisible ?? false,
    rightColumnVisible: false,
  };
}

function renderTopBar(overrides: TopBarOverrides = {}) {
  return render(<TopBar {...topBarProps(overrides)} />, { wrapper: MemoryRouter });
}

describe("TopBar", () => {
  it("renders the DEV badge only on a debug build", () => {
    const off = renderTopBar({ isDevBuild: false });
    expect(off.queryByLabelText("Debug build")).toBeNull();
    expect(off.queryByText("DEV")).toBeNull();
    off.unmount();
    const { getByText, getByLabelText } = renderTopBar({ isDevBuild: true });
    expect(getByText("DEV")).toBeTruthy();
    expect(getByLabelText("Debug build")).toBeTruthy();
  });

  it("bounds the header-start group: hidden below md, capped above, and hidden on the compact rail", () => {
    const group = (container: HTMLElement) => container.querySelector('[data-testid="topbar-header-start"]');
    const roomy = renderTopBar();
    expect(group(roomy.container)?.className).toMatch(/\bhidden\b.*\bmd:flex\b.*\bmax-w-48\b.*\boverflow-hidden\b/);
    expect(group(roomy.container)?.className).not.toContain("md:hidden");
    roomy.unmount();

    localStorage.setItem("aoe-web-settings", JSON.stringify({ sidebarCompact: true }));
    try {
      const compact = render(<TopBar {...topBarProps()} sidebarColumnVisible />, { wrapper: MemoryRouter });
      expect(group(compact.container)?.className).toContain("md:hidden");
      expect(group(compact.container)?.className).not.toContain("md:flex");
    } finally {
      localStorage.removeItem("aoe-web-settings");
    }
  });

  it("heads an open session with its project and title, as Claude Code desktop does", () => {
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
    const { getByTestId, rerender } = renderTopBar({
      activeWorkspace: workspace,
      activeSession: { title: "Fix the cart", project_path: "/home/user/breadcrumb-repo/" } as SessionResponse,
    });
    expect(getByTestId("session-breadcrumb").textContent).toBe("breadcrumb-repo/Fix the cart");
    rerender(<TopBar {...topBarProps({ activeWorkspace: undefined, activeSession: null })} />);
    expect(() => getByTestId("session-breadcrumb")).toThrow();
  });

  it("replaces the sidebar toggle and dashboard link with an All Sessions button in focus mode", () => {
    const onToggleSidebar = vi.fn();
    const onExitFocusMode = vi.fn();
    const { getByRole, queryByRole } = render(
      <TopBar {...topBarProps({ onToggleSidebar, needsYouCount: 1 })} onExitFocusMode={onExitFocusMode} />,
      { wrapper: MemoryRouter },
    );
    expect(queryByRole("button", { name: /Toggle sidebar/ })).toBeNull();
    expect(queryByRole("button", { name: "Go to dashboard" })).toBeNull();
    // The button is the only left control, and its visible label names what it brings back.
    const exit = getByRole("button", { name: "All Sessions, 1 needs you" });
    expect(exit.textContent).toContain("All Sessions");
    expect(exit.closest("header")!.querySelector('img[src="/icon-192.png"]')).toBeNull();
    fireEvent.click(exit);
    expect(onExitFocusMode).toHaveBeenCalledTimes(1);
    expect(onToggleSidebar).not.toHaveBeenCalled();
  });

  it("shows the needs-you count on the sidebar toggle only while the sidebar is hidden", () => {
    const zero = renderTopBar({ needsYouCount: 0 });
    expect(zero.queryByTestId("topbar-needs-you-badge")).toBeNull();
    expect(zero.getByRole("button", { name: "Toggle sidebar" })).toBeTruthy();
    zero.unmount();

    const shown = renderTopBar({ needsYouCount: 3, sidebarColumnVisible: true });
    expect(shown.queryByTestId("topbar-needs-you-badge")).toBeNull();
    expect(shown.getByRole("button", { name: "Toggle sidebar" })).toBeTruthy();
    shown.unmount();

    const onToggleSidebar = vi.fn();
    const { getByTestId, getByRole } = renderTopBar({ needsYouCount: 3, onToggleSidebar });
    expect(getByTestId("topbar-needs-you-badge").textContent).toBe("3");
    expect(getByRole("button", { name: "Toggle sidebar, 3 need you" })).toBeTruthy();
    // The badge nests inside the toggle, so tapping it toggles the sidebar.
    fireEvent.click(getByTestId("topbar-needs-you-badge"));
    expect(onToggleSidebar).toHaveBeenCalledTimes(1);
  });

  it("announces count changes through a persistent live region, including the return to zero", () => {
    const { getByTestId, rerender } = renderTopBar({ needsYouCount: 0 });
    const live = getByTestId("topbar-attention-live-region");
    expect(live.getAttribute("aria-live")).toBe("polite");
    expect(live.textContent).toBe("0 need you");

    rerender(<TopBar {...topBarProps({ needsYouCount: 2 })} />);
    expect(getByTestId("topbar-attention-live-region").textContent).toBe("2 need you");

    // Clearing the last one is a real text mutation (not an empty string), so screen readers announce it too.
    rerender(<TopBar {...topBarProps({ needsYouCount: 0 })} />);
    expect(getByTestId("topbar-attention-live-region").textContent).toBe("0 need you");
  });
});

describe("polling connectivity consumer", () => {
  it("says the Daemon is unreachable on an unanswered first poll, then clears it when the replacement succeeds", async () => {
    vi.useFakeTimers();
    const signals: AbortSignal[] = [];
    vi.stubGlobal(
      "fetch",
      vi
        .fn(
          (_url, init?: RequestInit) =>
            new Promise<Response>((_resolve, reject) => {
              signals.push(init!.signal as AbortSignal);
              init!.signal!.addEventListener("abort", () => reject(new DOMException("aborted", "AbortError")), {
                once: true,
              });
            }),
        )
        .mockImplementationOnce(
          (_url, init?: RequestInit) =>
            new Promise<Response>((_resolve, reject) => {
              signals.push(init!.signal as AbortSignal);
              init!.signal!.addEventListener("abort", () => reject(new DOMException("aborted", "AbortError")), {
                once: true,
              });
            }),
        )
        .mockImplementationOnce(
          async () =>
            new Response(JSON.stringify({ sessions: [], workspace_ordering: [] }), {
              headers: { "aoe-runtime-epoch": "boot", "aoe-runtime-revision": "1" },
            }),
        ),
    );
    // The app's one statement of it: the Daemon banner, which the header no longer repeats.
    function Header() {
      useSessions();
      const connection = useConnectionBanner();
      return connection === "down" ? <DisconnectBanner state="down" /> : null;
    }
    const view = render(
      <MemoryRouter>
        <Header />
      </MemoryRouter>,
    );
    const down = () => view.queryByText("Daemon unreachable. Reconnecting…");
    expect(down()).toBeNull();
    await act(async () => {
      await vi.advanceTimersByTimeAsync(15000);
    });
    expect(signals[0]!.aborted).toBe(true);
    expect(down()).toBeTruthy();
    await act(async () => {
      await vi.advanceTimersByTimeAsync(3000);
    });
    expect(down()).toBeNull();
  });
});
