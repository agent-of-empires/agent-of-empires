// @vitest-environment jsdom
//
// Covers the `loaded` sentinel added for #1351. Without `loaded` the
// session-route gate in App.tsx cannot tell "first fetch still in
// flight" apart from "server confirmed there is no such session,"
// which is why refresh on /session/<id> used to flash the dashboard.

import { renderHook, waitFor } from "@testing-library/react";
import { afterEach, describe, expect, it, vi } from "vitest";

import { useSessions } from "./useSessions";
import * as api from "../lib/api";

describe("useSessions / loaded sentinel", () => {
  afterEach(() => {
    vi.restoreAllMocks();
  });

  it("starts loaded=false before the first fetch resolves", () => {
    let resolveFetch: (value: api.SessionsEnvelope | null) => void = () => {};
    vi.spyOn(api, "fetchSessions").mockImplementation(() => new Promise((r) => (resolveFetch = r)));

    const { result } = renderHook(() => useSessions());

    expect(result.current.loaded).toBe(false);
    expect(result.current.sessions).toEqual([]);
    // Resolve the pending promise so the polling effect's cleanup
    // does not leak past this test.
    resolveFetch(null);
  });

  it("flips loaded=true after the first successful fetch", async () => {
    vi.spyOn(api, "fetchSessions").mockResolvedValue({
      sessions: [],
      workspace_ordering: [],
    });

    const { result } = renderHook(() => useSessions());

    await waitFor(() => expect(result.current.loaded).toBe(true));
    expect(result.current.error).toBe(false);
  });

  it("flips loaded=true even when the first fetch returns null", async () => {
    vi.spyOn(api, "fetchSessions").mockResolvedValue(null);

    const { result } = renderHook(() => useSessions());

    await waitFor(() => expect(result.current.loaded).toBe(true));
    // The null branch also flags error/serverDown surfaces; the
    // important contract for #1351 is that `loaded` does not stay
    // stuck at false, so the App.tsx gate can release the placeholder.
    expect(result.current.error).toBe(true);
  });
});
