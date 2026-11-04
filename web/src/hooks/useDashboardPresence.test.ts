// @vitest-environment jsdom

import { act, renderHook } from "@testing-library/react";
import { afterEach, describe, expect, it, vi } from "vitest";

import { useDashboardPresence } from "./useDashboardPresence";

type PresenceBody = { active: boolean; window: string; focus_session: string | null };

const lastBody = (fetchMock: ReturnType<typeof vi.fn>): PresenceBody =>
  JSON.parse((fetchMock.mock.lastCall![1] as RequestInit).body as string) as PresenceBody;

describe("useDashboardPresence", () => {
  afterEach(() => {
    vi.restoreAllMocks();
    vi.unstubAllGlobals();
  });

  it("reports foreground use, then clears presence when the dashboard is hidden", () => {
    let visible = true;
    vi.spyOn(document, "visibilityState", "get").mockImplementation(() => (visible ? "visible" : "hidden"));
    vi.spyOn(document, "hasFocus").mockReturnValue(true);
    const fetchMock = vi.fn().mockResolvedValue(new Response());
    vi.stubGlobal("fetch", fetchMock);

    const { unmount, rerender } = renderHook(({ focus }) => useDashboardPresence(focus), {
      initialProps: { focus: null as string | null },
    });
    expect(fetchMock).toHaveBeenLastCalledWith("/api/presence", expect.objectContaining({ keepalive: false }));
    const first = lastBody(fetchMock);
    expect(first).toMatchObject({ active: true, focus_session: null });
    expect(first.window).not.toBe("");

    // A focus change re-reports under the same window id, without clearing first: an out-of-order clear would drop
    // the new state on the server until the next heartbeat.
    for (const focus of ["s1", null]) {
      const before = fetchMock.mock.calls.length;
      rerender({ focus });
      expect(fetchMock.mock.calls).toHaveLength(before + 1);
      expect(lastBody(fetchMock)).toEqual({ active: true, window: first.window, focus_session: focus });
    }
    rerender({ focus: "s1" });

    visible = false;
    act(() => document.dispatchEvent(new Event("visibilitychange")));
    expect(fetchMock).toHaveBeenLastCalledWith("/api/presence", expect.objectContaining({ keepalive: true }));
    expect(lastBody(fetchMock)).toMatchObject({ active: false, focus_session: "s1" });

    unmount();
  });
});
