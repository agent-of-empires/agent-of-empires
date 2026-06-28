// Vitest coverage for the trash/restore API clients (#2489): both return the
// fresh SessionResponse on success and null on a non-ok response or a thrown
// fetch, which is the signal callers use to flag a failed trash/restore.

import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import { restoreSession, trashSession } from "../api";
import type { SessionResponse } from "../types";

const fetchSpy = vi.fn<typeof fetch>();

beforeEach(() => {
  fetchSpy.mockReset();
  vi.stubGlobal("fetch", fetchSpy);
});

afterEach(() => {
  vi.unstubAllGlobals();
});

const snapshot = { id: "s1", title: "t", trashed_at: "2026-01-01T00:00:00Z" } as unknown as SessionResponse;

describe("trashSession (#2489)", () => {
  it("POSTs kill_pane and returns the snapshot on success", async () => {
    fetchSpy.mockResolvedValue(new Response(JSON.stringify(snapshot), { status: 200 }));

    const res = await trashSession("s1");

    expect(res).toEqual(snapshot);
    const [url, init] = fetchSpy.mock.calls[0];
    expect(url).toBe("/api/sessions/s1/trash");
    expect(init?.method).toBe("POST");
    expect(JSON.parse(String(init?.body))).toEqual({ kill_pane: true });
  });

  it("forwards kill_pane=false", async () => {
    fetchSpy.mockResolvedValue(new Response(JSON.stringify(snapshot), { status: 200 }));
    await trashSession("s1", false);
    expect(JSON.parse(String(fetchSpy.mock.calls[0][1]?.body))).toEqual({ kill_pane: false });
  });

  it("returns null on a non-ok response", async () => {
    fetchSpy.mockResolvedValue(new Response("nope", { status: 500 }));
    expect(await trashSession("s1")).toBeNull();
  });

  it("returns null when fetch throws", async () => {
    fetchSpy.mockRejectedValue(new Error("offline"));
    expect(await trashSession("s1")).toBeNull();
  });
});

describe("restoreSession (#2489)", () => {
  it("POSTs and returns the snapshot on success", async () => {
    const restored = { ...snapshot, trashed_at: null } as SessionResponse;
    fetchSpy.mockResolvedValue(new Response(JSON.stringify(restored), { status: 200 }));

    const res = await restoreSession("s1");

    expect(res).toEqual(restored);
    expect(fetchSpy.mock.calls[0][0]).toBe("/api/sessions/s1/restore");
    expect(fetchSpy.mock.calls[0][1]?.method).toBe("POST");
  });

  it("returns null on a non-ok response", async () => {
    fetchSpy.mockResolvedValue(new Response("nope", { status: 404 }));
    expect(await restoreSession("s1")).toBeNull();
  });

  it("returns null when fetch throws", async () => {
    fetchSpy.mockRejectedValue(new Error("offline"));
    expect(await restoreSession("s1")).toBeNull();
  });
});
