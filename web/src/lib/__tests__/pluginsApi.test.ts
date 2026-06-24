// Vitest coverage for the plugin API client (#268): the GET /api/plugins read
// and the POST enable/disable toggle. The toggle validates the success payload
// shape before reporting ok, and degrades every failure (non-OK, malformed
// body, network throw) to a typed error rather than throwing.

import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import { fetchPlugins, setPluginEnabled, updateSettings } from "../api";

const fetchSpy = vi.fn<typeof fetch>();

beforeEach(() => {
  fetchSpy.mockReset();
  vi.stubGlobal("fetch", fetchSpy);
});

afterEach(() => {
  vi.unstubAllGlobals();
});

const listPayload = {
  plugins: [{ id: "aoe.web", name: "Web Dashboard", version: "1.0.0", description: "", enabled: true, builtin: true }],
  load_errors: [],
};

describe("fetchPlugins", () => {
  it("returns the parsed list from GET /api/plugins", async () => {
    fetchSpy.mockResolvedValue(new Response(JSON.stringify(listPayload), { status: 200 }));
    expect(await fetchPlugins()).toEqual(listPayload);
    expect(fetchSpy.mock.calls[0][0]).toBe("/api/plugins");
  });

  it("returns null on a non-OK response", async () => {
    fetchSpy.mockResolvedValue(new Response("nope", { status: 500 }));
    expect(await fetchPlugins()).toBeNull();
  });

  it("returns null when the request throws", async () => {
    fetchSpy.mockRejectedValue(new Error("offline"));
    expect(await fetchPlugins()).toBeNull();
  });
});

describe("setPluginEnabled", () => {
  it("POSTs the enabled flag and returns the refreshed list on success", async () => {
    fetchSpy.mockResolvedValue(new Response(JSON.stringify(listPayload), { status: 200 }));
    const result = await setPluginEnabled("aoe.web", false);

    expect(result).toEqual({ kind: "ok", data: listPayload });
    const [url, init] = fetchSpy.mock.calls[0];
    expect(url).toBe("/api/plugins/aoe.web/enabled");
    expect(init?.method).toBe("POST");
    expect(JSON.parse(String(init?.body))).toEqual({ enabled: false });
  });

  it("url-encodes the plugin id", async () => {
    fetchSpy.mockResolvedValue(new Response(JSON.stringify(listPayload), { status: 200 }));
    await setPluginEnabled("acme/weird id", true);
    expect(fetchSpy.mock.calls[0][0]).toBe("/api/plugins/acme%2Fweird%20id/enabled");
  });

  it("reports an error when an OK response has a malformed shape", async () => {
    fetchSpy.mockResolvedValue(new Response(JSON.stringify({ nope: true }), { status: 200 }));
    const result = await setPluginEnabled("aoe.web", true);
    expect(result.kind).toBe("error");
  });

  it("surfaces the server message on a non-OK response", async () => {
    fetchSpy.mockResolvedValue(
      new Response(JSON.stringify({ error: "plugin_error", message: "boom" }), { status: 400 }),
    );
    const result = await setPluginEnabled("aoe.web", true);
    expect(result).toEqual({ kind: "error", message: "boom" });
  });

  it("falls back to a status message when the error body has none", async () => {
    fetchSpy.mockResolvedValue(new Response("not json", { status: 403 }));
    const result = await setPluginEnabled("aoe.web", false);
    expect(result).toEqual({ kind: "error", message: "Failed to disable plugin (403)." });
  });

  it("returns a network error when the request throws", async () => {
    fetchSpy.mockRejectedValue(new Error("offline"));
    const result = await setPluginEnabled("aoe.web", true);
    expect(result).toEqual({ kind: "error", message: "Network error." });
  });
});

describe("updateSettings", () => {
  it("PATCHes /api/settings and returns true on success", async () => {
    fetchSpy.mockResolvedValue(new Response("", { status: 200 }));
    expect(await updateSettings({ theme: { name: "x" } })).toBe(true);
    const [url, init] = fetchSpy.mock.calls[0];
    expect(url).toBe("/api/settings");
    expect(init?.method).toBe("PATCH");
  });

  it("returns false on a non-OK response", async () => {
    fetchSpy.mockResolvedValue(new Response("denied", { status: 403 }));
    expect(await updateSettings({})).toBe(false);
  });

  it("returns false when the request throws", async () => {
    fetchSpy.mockRejectedValue(new Error("offline"));
    expect(await updateSettings({})).toBe(false);
  });
});
