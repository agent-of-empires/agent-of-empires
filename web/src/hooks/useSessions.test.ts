// @vitest-environment jsdom

import { renderHook, waitFor } from "@testing-library/react";
import { afterEach, describe, expect, it, vi } from "vitest";

import { useAppliedRead } from "../lib/sessionReads";
import type { SessionResponse } from "../lib/types";
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
    resolveFetch(null);
  });

  it.each([
    ["a successful fetch", { sessions: [], workspace_ordering: [] }, false],
    ["a failed fetch", null, true],
  ] as [string, api.SessionsEnvelope | null, boolean][])(
    "flips loaded=true after %s",
    async (_label, envelope, error) => {
      vi.spyOn(api, "fetchSessions").mockResolvedValue(envelope);
      const { result } = renderHook(() => useSessions());
      await waitFor(() => expect(result.current.loaded).toBe(true));
      expect(result.current.error).toBe(error);
    },
  );
});

describe("useSessions / read ordinal", () => {
  afterEach(() => {
    vi.restoreAllMocks();
  });

  it("never renders a newer ordinal beside the previous snapshot", async () => {
    const session = { id: "s1" } as SessionResponse;
    vi.spyOn(api, "fetchSessions").mockResolvedValue({ sessions: [session], workspace_ordering: [] });
    const renders: { sessions: number; read: number }[] = [];
    const { result } = renderHook(() => {
      const { sessions } = useSessions();
      const read = useAppliedRead();
      renders.push({ sessions: sessions.length, read });
      return read;
    });
    const before = renders[0]!.read;
    await waitFor(() => expect(result.current).toBeGreaterThan(before));
    expect(renders.filter((r) => r.read > before && r.sessions === 0)).toEqual([]);
  });
});
