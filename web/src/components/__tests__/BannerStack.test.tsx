// @vitest-environment jsdom

import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { act, cleanup, render, screen, waitFor } from "@testing-library/react";

import type { ServerAbout, UpdateStatus } from "../../lib/api";
import type { PushHealth } from "../../lib/pushHealth";
import { setServerDown } from "../../lib/connectionState";
import type { ConnectionBannerState } from "../../hooks/useConnectionBanner";

const fetchUpdateStatus = vi.fn();
const fetchAbout = vi.fn();
vi.mock("../../lib/api", () => ({
  fetchUpdateStatus: (...args: unknown[]) => fetchUpdateStatus(...args),
  fetchAbout: (...args: unknown[]) => fetchAbout(...args),
  dismissUpdate: () => Promise.resolve(true),
}));

let pushHealth: PushHealth = "healthy";
vi.mock("../../hooks/usePushSubscription", () => ({
  usePushSubscription: () => ({ state: { kind: "off" }, health: pushHealth, enable: vi.fn() }),
}));

// Null means the real hook runs.
let forced: { connection: ConnectionBannerState | null; dashboard: boolean; update: boolean } | null = null;
vi.mock("../../hooks/useConnectionBanner", async (importOriginal) => {
  const mod = await importOriginal<typeof import("../../hooks/useConnectionBanner")>();
  return { ...mod, useConnectionBanner: () => (forced ? forced.connection : mod.useConnectionBanner()) };
});
vi.mock("../../hooks/useDashboardUpdate", async (importOriginal) => {
  const mod = await importOriginal<typeof import("../../hooks/useDashboardUpdate")>();
  return { ...mod, useDashboardUpdate: () => (forced ? forced.dashboard : mod.useDashboardUpdate()) };
});
vi.mock("../../hooks/useUpdateBanner", async (importOriginal) => {
  const mod = await importOriginal<typeof import("../../hooks/useUpdateBanner")>();
  return {
    ...mod,
    useUpdateBanner: () =>
      forced ? (forced.update ? { status: status(), dismiss: () => {} } : null) : mod.useUpdateBanner(),
  };
});

import { BannerStack } from "../BannerStack";

function status(overrides?: Partial<UpdateStatus>): UpdateStatus {
  return {
    update_check_mode: "notify",
    current_version: "1.0.0",
    latest_version: "1.1.0",
    update_available: true,
    release_url: null,
    error: null,
    dismissed_version: null,
    ...overrides,
  };
}

function about(webBuildId: string): ServerAbout {
  return { web_build_id: webBuildId } as ServerAbout;
}

const NAMES: [RegExp, string][] = [
  [/Server unreachable/, "disconnected"],
  [/Dashboard updated/, "dashboard updated"],
  [/Reconnected/, "reconnected"],
  [/Update available/, "update available"],
  [/Notifications stopped/, "push"],
];

function shown(container: HTMLElement): string[] {
  // Top-level banners only; a banner's own copy feedback is a status region inside it.
  return Array.from(container.querySelectorAll(':scope > [role="alert"], :scope > [role="status"]')).map((el) => {
    const text = `${el.getAttribute("aria-label") ?? ""} ${el.textContent ?? ""}`;
    return NAMES.find(([re]) => re.test(text))?.[1] ?? text;
  });
}

beforeEach(() => {
  const script = document.createElement("script");
  script.type = "module";
  script.src = "/assets/index-PageBuild.js";
  document.head.appendChild(script);
  fetchUpdateStatus.mockReset();
  fetchAbout.mockReset();
  pushHealth = "healthy";
  forced = null;
  setServerDown(false);
});

afterEach(() => {
  cleanup();
  setServerDown(false);
  document.head.querySelectorAll("script").forEach((s) => s.remove());
});

describe("BannerStack", () => {
  it("shows one connection or update banner, most urgent first, and push only while connected", () => {
    // (connection, dashboard updated, update available, push unhealthy, banners shown top to bottom)
    const cases: [ConnectionBannerState | null, boolean, boolean, boolean, string[]][] = [
      [null, false, false, false, []],
      [null, false, true, true, ["update available", "push"]],
      ["down", true, true, true, ["disconnected"]],
      ["reconnected", false, true, true, ["reconnected", "push"]],
      ["reconnected", true, true, false, ["dashboard updated"]],
      [null, true, true, true, ["dashboard updated", "push"]],
    ];
    for (const [connection, dashboard, update, pushUnhealthy, want] of cases) {
      forced = { connection, dashboard, update };
      pushHealth = pushUnhealthy ? "revoked" : "healthy";
      const { container, unmount } = render(<BannerStack />);
      expect(shown(container), JSON.stringify({ connection, dashboard, update, pushUnhealthy })).toEqual(want);
      unmount();
    }
  });

  it("re-checks both updates once on reconnect, and a new bundle replaces the stale offer", async () => {
    fetchUpdateStatus.mockResolvedValue(status());
    fetchAbout.mockResolvedValue(about("index-PageBuild.js"));
    const { container } = render(<BannerStack />);
    await screen.findByRole("status", { name: /Update available/ });
    expect([fetchUpdateStatus.mock.calls.length, fetchAbout.mock.calls.length]).toEqual([1, 1]);

    act(() => setServerDown(true));
    expect(shown(container)).toEqual(["disconnected"]);

    // The Daemon restarted into the update: the offer is gone and the bundle changed. The reconnect lands
    // inside the about check's 30 s throttle, so only the forced re-check can see it.
    fetchUpdateStatus.mockResolvedValue(status({ update_available: false }));
    fetchAbout.mockResolvedValue(about("index-NewerBuild.js"));
    act(() => setServerDown(false));
    await screen.findByRole("status", { name: "Dashboard update available" });
    await waitFor(() => expect(fetchUpdateStatus).toHaveBeenCalledTimes(2));
    expect(fetchAbout).toHaveBeenCalledTimes(2);
    expect(shown(container)).toEqual(["dashboard updated"]);
  });
});
