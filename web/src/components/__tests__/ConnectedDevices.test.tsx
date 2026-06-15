// @vitest-environment jsdom
//
// Tests for ConnectedDevices. The component fetches the list of signed-in
// devices on mount (via a deferred setTimeout, plus a 10s polling interval
// and a visibilitychange listener), renders a loading / empty / populated /
// error state, and exposes per-device "Revoke" and a global "Sign out all"
// affordance, both elevation-gated through api helpers we mock here.
//
// Fake timers drive the deferred first load and the polling interval
// deterministically. testing-library's `waitFor` polls on a real-timer
// interval that never advances under fake timers, so instead of `waitFor`
// we flush the component's own timers + microtasks with
// `act(() => vi.advanceTimersByTimeAsync(...))` and then assert synchronously.

import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { act, cleanup, fireEvent, render, screen } from "@testing-library/react";

import { ConnectedDevices } from "../ConnectedDevices";
import type { DeviceSession } from "../../lib/api";

vi.mock("../../lib/api", () => ({
  fetchDevices: vi.fn(),
  revokeDevice: vi.fn(),
  signOutAllDevices: vi.fn(),
}));

import { fetchDevices, revokeDevice, signOutAllDevices } from "../../lib/api";

const mockFetchDevices = vi.mocked(fetchDevices);
const mockRevokeDevice = vi.mocked(revokeDevice);
const mockSignOutAllDevices = vi.mocked(signOutAllDevices);

function device(overrides: Partial<DeviceSession> = {}): DeviceSession {
  const now = new Date().toISOString();
  return {
    session_id: "sess-1",
    user_agent: "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15) Chrome/120.0",
    created_ip: "192.168.1.10",
    created_at: now,
    last_seen: now,
    current: false,
    ...overrides,
  };
}

beforeEach(() => {
  vi.clearAllMocks();
  vi.useFakeTimers();
});

afterEach(() => {
  cleanup();
  vi.runOnlyPendingTimers();
  // Restore at teardown so a window.confirm spy can't leak into later tests
  // if an assertion throws before the per-test mockRestore runs.
  vi.restoreAllMocks();
  vi.useRealTimers();
});

/** Advance the timers far enough to fire the deferred first load
 *  (setTimeout(load, 0)) and flush the awaited fetch promise + state update,
 *  all inside act() so React applies the update before we assert. */
async function flush(ms = 0) {
  await act(async () => {
    await vi.advanceTimersByTimeAsync(ms);
  });
}

async function renderAndLoad() {
  const utils = render(<ConnectedDevices />);
  await flush(0);
  return utils;
}

function revokeButtons(container: HTMLElement): HTMLButtonElement[] {
  return Array.from(container.querySelectorAll("button")).filter((b) => b.textContent?.trim() === "Revoke");
}

describe("ConnectedDevices", () => {
  it("shows the loading state before the first fetch resolves", () => {
    // Never-resolving fetch so the component stays in its initial null state.
    mockFetchDevices.mockReturnValue(new Promise<never>(() => {}));
    render(<ConnectedDevices />);
    expect(screen.getByText("Loading...")).toBeTruthy();
    expect(screen.getByText("Connected Devices")).toBeTruthy();
  });

  it("renders the empty state when no devices are signed in", async () => {
    mockFetchDevices.mockResolvedValue([]);
    await renderAndLoad();
    expect(screen.getByText("No signed-in devices")).toBeTruthy();
    expect(screen.queryByText("Loading...")).toBeNull();
  });

  it("renders the error state when the fetch fails (returns null)", async () => {
    mockFetchDevices.mockResolvedValue(null);
    await renderAndLoad();
    expect(screen.getByText("Could not load devices")).toBeTruthy();
    expect(screen.queryByText("Loading...")).toBeNull();
  });

  it("renders a populated list flagging the current device and showing a Revoke button only for others", async () => {
    mockFetchDevices.mockResolvedValue([
      device({ session_id: "me", current: true, user_agent: "Mozilla/5.0 (iPhone) Safari/605" }),
      device({ session_id: "other", current: false, created_ip: "10.0.0.5" }),
    ]);
    const { container } = await renderAndLoad();

    expect(screen.getByText("this device")).toBeTruthy();
    // Current device shows its parsed UA; the other shows its IP.
    expect(screen.getByText("Safari · iOS")).toBeTruthy();
    expect(screen.getByText("10.0.0.5")).toBeTruthy();
    expect(screen.getAllByText(/last seen:/).length).toBe(2);

    // Exactly one Revoke button (the non-current device). The current device
    // has no Revoke control.
    expect(revokeButtons(container)).toHaveLength(1);
  });

  it("revokes a device by id and reloads the list", async () => {
    mockFetchDevices
      .mockResolvedValueOnce([
        device({ session_id: "me", current: true }),
        device({ session_id: "other", current: false }),
      ])
      .mockResolvedValueOnce([device({ session_id: "me", current: true })]);
    mockRevokeDevice.mockResolvedValue(true);

    const { container } = await renderAndLoad();
    expect(screen.getByText("this device")).toBeTruthy();

    const [revokeBtn] = revokeButtons(container);
    expect(revokeBtn).toBeTruthy();

    fireEvent.click(revokeBtn);
    await flush(0);

    expect(mockRevokeDevice).toHaveBeenCalledTimes(1);
    expect(mockRevokeDevice).toHaveBeenCalledWith("other");

    // After the reload the only device left is the current one, so no Revoke
    // button remains.
    expect(revokeButtons(container)).toHaveLength(0);
    expect(mockFetchDevices).toHaveBeenCalledTimes(2);
  });

  it("does not reload the list when revoke fails (resolves false)", async () => {
    mockFetchDevices.mockResolvedValue([
      device({ session_id: "me", current: true }),
      device({ session_id: "other", current: false }),
    ]);
    mockRevokeDevice.mockResolvedValue(false);

    const { container } = await renderAndLoad();
    const [revokeBtn] = revokeButtons(container);

    fireEvent.click(revokeBtn);
    await flush(0);

    expect(mockRevokeDevice).toHaveBeenCalledWith("other");
    // Only the initial fetch ran; no reload because revoke returned false.
    expect(mockFetchDevices).toHaveBeenCalledTimes(1);
  });

  it("signs out all devices after confirmation and reloads", async () => {
    mockFetchDevices.mockResolvedValueOnce([device({ session_id: "me", current: true })]).mockResolvedValueOnce([]);
    mockSignOutAllDevices.mockResolvedValue(true);
    const confirmSpy = vi.spyOn(window, "confirm").mockReturnValue(true);

    const { container } = await renderAndLoad();
    expect(screen.getByText("this device")).toBeTruthy();

    const signOutBtn = Array.from(container.querySelectorAll("button")).find((b) =>
      b.textContent?.includes("Sign out all devices"),
    )!;
    expect(signOutBtn).toBeTruthy();

    fireEvent.click(signOutBtn);
    await flush(0);

    expect(confirmSpy).toHaveBeenCalledTimes(1);
    expect(mockSignOutAllDevices).toHaveBeenCalledTimes(1);
    expect(mockFetchDevices).toHaveBeenCalledTimes(2);
    expect(screen.getByText("No signed-in devices")).toBeTruthy();
  });

  it("aborts sign-out-all when the confirm dialog is dismissed", async () => {
    mockFetchDevices.mockResolvedValue([device({ session_id: "me", current: true })]);
    const confirmSpy = vi.spyOn(window, "confirm").mockReturnValue(false);

    const { container } = await renderAndLoad();
    const signOutBtn = Array.from(container.querySelectorAll("button")).find((b) =>
      b.textContent?.includes("Sign out all devices"),
    )!;

    fireEvent.click(signOutBtn);
    await flush(0);

    expect(confirmSpy).toHaveBeenCalledTimes(1);
    expect(mockSignOutAllDevices).not.toHaveBeenCalled();
    expect(mockFetchDevices).toHaveBeenCalledTimes(1);
  });

  it("re-fetches devices when the polling interval fires", async () => {
    mockFetchDevices.mockResolvedValue([device({ session_id: "me", current: true })]);
    await renderAndLoad();
    expect(screen.getByText("this device")).toBeTruthy();
    expect(mockFetchDevices).toHaveBeenCalledTimes(1);

    await flush(10_000);
    expect(mockFetchDevices).toHaveBeenCalledTimes(2);
  });

  it("re-fetches when the tab becomes visible again", async () => {
    mockFetchDevices.mockResolvedValue([device({ session_id: "me", current: true })]);
    await renderAndLoad();
    expect(mockFetchDevices).toHaveBeenCalledTimes(1);

    // jsdom defaults visibilityState to "visible".
    await act(async () => {
      fireEvent(document, new Event("visibilitychange"));
      await vi.advanceTimersByTimeAsync(0);
    });
    expect(mockFetchDevices).toHaveBeenCalledTimes(2);
  });

  it("parses various user agents into a Browser · OS label and marks stale devices", async () => {
    const old = new Date(Date.now() - 2 * 3_600_000).toISOString();
    mockFetchDevices.mockResolvedValue([
      device({ session_id: "a", user_agent: "Mozilla/5.0 Firefox/120.0 Windows NT 10.0", last_seen: old }),
      device({ session_id: "b", user_agent: "curl/8.4.0 Linux" }),
    ]);
    await renderAndLoad();

    expect(screen.getByText("Firefox · Windows")).toBeTruthy();
    expect(screen.getByText("curl · Linux")).toBeTruthy();
    // The stale device (2h old) renders an "h ago" relative time.
    expect(screen.getByText(/last seen: 2h ago/)).toBeTruthy();
  });
});
