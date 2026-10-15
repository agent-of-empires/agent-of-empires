// @vitest-environment jsdom

import { act, renderHook } from "@testing-library/react";
import { afterEach, describe, expect, it, vi } from "vitest";

import { useDashboardPresence } from "./useDashboardPresence";

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

    const { unmount } = renderHook(() => useDashboardPresence());
    expect(fetchMock).toHaveBeenLastCalledWith(
      "/api/presence",
      expect.objectContaining({ body: '{"active":true}', keepalive: false }),
    );

    visible = false;
    act(() => document.dispatchEvent(new Event("visibilitychange")));
    expect(fetchMock).toHaveBeenLastCalledWith(
      "/api/presence",
      expect.objectContaining({ body: '{"active":false}', keepalive: true }),
    );

    unmount();
  });
});
