// Vitest coverage for the tips API client (#2292): the GET /api/tips read and
// the two single-purpose writes (mark a tip seen, set the show-on-startup
// preference). All swallow network and non-OK responses so the dashboard
// degrades to no badge / no-op rather than throwing.

import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import { fetchTips, markTipSeen, setShowTips } from "../api";

const fetchSpy = vi.fn<typeof fetch>();

beforeEach(() => {
  fetchSpy.mockReset();
  vi.stubGlobal("fetch", fetchSpy);
});

afterEach(() => {
  vi.unstubAllGlobals();
});

describe("fetchTips", () => {
  it("returns the parsed payload from GET /api/tips", async () => {
    const payload = {
      enabled: true,
      tips: [{ id: "install-dashboard-pwa", title: "T", body: "B", seen: false }],
    };
    fetchSpy.mockResolvedValue(new Response(JSON.stringify(payload), { status: 200 }));

    expect(await fetchTips()).toEqual(payload);
    expect(fetchSpy.mock.calls[0][0]).toBe("/api/tips");
  });

  it("returns null on a non-OK response", async () => {
    fetchSpy.mockResolvedValue(new Response("nope", { status: 500 }));
    expect(await fetchTips()).toBeNull();
  });

  it("returns null when the request throws", async () => {
    fetchSpy.mockRejectedValue(new Error("network down"));
    expect(await fetchTips()).toBeNull();
  });
});

describe("markTipSeen", () => {
  it("POSTs the tip id and returns true on success", async () => {
    fetchSpy.mockResolvedValue(new Response("{}", { status: 200 }));

    expect(await markTipSeen("pin-sessions")).toBe(true);
    const [url, init] = fetchSpy.mock.calls[0];
    expect(url).toBe("/api/app-state/tip-seen");
    expect(init?.method).toBe("POST");
    expect(JSON.parse(String(init?.body))).toEqual({ id: "pin-sessions" });
  });

  it("returns false on a non-OK response (e.g. read-only 403)", async () => {
    fetchSpy.mockResolvedValue(new Response("forbidden", { status: 403 }));
    expect(await markTipSeen("x")).toBe(false);
  });

  it("returns false when the request throws", async () => {
    fetchSpy.mockRejectedValue(new Error("network down"));
    expect(await markTipSeen("x")).toBe(false);
  });
});

describe("setShowTips", () => {
  it("POSTs the enabled flag and returns true on success", async () => {
    fetchSpy.mockResolvedValue(new Response("{}", { status: 200 }));

    expect(await setShowTips(false)).toBe(true);
    const [url, init] = fetchSpy.mock.calls[0];
    expect(url).toBe("/api/tips/show");
    expect(init?.method).toBe("POST");
    expect(JSON.parse(String(init?.body))).toEqual({ enabled: false });
  });

  it("returns false on a non-OK response", async () => {
    fetchSpy.mockResolvedValue(new Response("forbidden", { status: 403 }));
    expect(await setShowTips(true)).toBe(false);
  });

  it("returns false when the request throws", async () => {
    fetchSpy.mockRejectedValue(new Error("network down"));
    expect(await setShowTips(true)).toBe(false);
  });
});
