// Vitest coverage for the plugin API client (#268): the GET /api/plugins read
// and the POST enable/disable toggle. The toggle validates the success payload
// shape before reporting ok, and degrades every failure (non-OK, malformed
// body, network throw) to a typed error rather than throwing.

import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import {
  applyPluginUpdate,
  dismissPluginUpdate,
  fetchPlugins,
  invokePluginCommand,
  previewPluginUpdate,
  resolvePluginOptions,
  setPluginEnabled,
  updateSettings,
} from "../api";

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

describe("invokePluginCommand", () => {
  it("POSTs the session id to the invoke endpoint and returns true on ok", async () => {
    fetchSpy.mockResolvedValue(new Response(JSON.stringify({ ok: true }), { status: 202 }));
    expect(await invokePluginCommand("plugin.acme.gh.refresh", "s1")).toBe(true);
    const [url, init] = fetchSpy.mock.calls[0];
    expect(url).toBe("/api/plugins/commands/plugin.acme.gh.refresh/invoke");
    expect(init?.method).toBe("POST");
    expect(JSON.parse(String(init?.body))).toEqual({ session_id: "s1" });
  });

  it("returns false on a non-OK response", async () => {
    fetchSpy.mockResolvedValue(new Response("no", { status: 404 }));
    expect(await invokePluginCommand("plugin.acme.gh.refresh", "s1")).toBe(false);
  });

  it("returns false when the request throws", async () => {
    fetchSpy.mockRejectedValue(new Error("offline"));
    expect(await invokePluginCommand("plugin.acme.gh.refresh", "s1")).toBe(false);
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

const consentPreview = {
  kind: "consent_required",
  dismissed: false,
  consent: {
    id: "acme.plugin",
    from_version: "1.0.0",
    to_version: "2.0.0",
    prior_capabilities: ["net"],
    new_capabilities: ["net", "fs.read"],
    added_capabilities: ["fs.read"],
    removed_capabilities: [],
    ui: [],
    build_steps: [],
    runtime_change: null,
    trust_downgrade: false,
    fingerprint: "treeB||community",
    stays_active_if_declined: true,
  },
};

describe("previewPluginUpdate", () => {
  it("returns the parsed preview from GET .../update/preview", async () => {
    fetchSpy.mockResolvedValue(new Response(JSON.stringify(consentPreview), { status: 200 }));
    const res = await previewPluginUpdate("acme.plugin");
    expect(res).toEqual({ kind: "ok", preview: consentPreview });
    expect(fetchSpy.mock.calls[0][0]).toBe("/api/plugins/acme.plugin/update/preview");
  });

  it("surfaces the server message on a non-OK response", async () => {
    fetchSpy.mockResolvedValue(new Response(JSON.stringify({ message: "no release" }), { status: 502 }));
    expect(await previewPluginUpdate("acme.plugin")).toEqual({ kind: "error", message: "no release" });
  });

  it("returns a network error when the request throws", async () => {
    fetchSpy.mockRejectedValue(new Error("offline"));
    expect(await previewPluginUpdate("acme.plugin")).toEqual({ kind: "error", message: "Network error." });
  });

  it("rejects a malformed OK payload that drops the per-kind required fields", async () => {
    // safe_update without a fingerprint, consent_required without a consent
    // object, and an unknown kind must all be treated as errors, not passed on.
    for (const bad of [
      { kind: "safe_update", to_version: "2.0.0" },
      { kind: "consent_required", dismissed: false },
      { kind: "bogus" },
    ]) {
      fetchSpy.mockResolvedValue(new Response(JSON.stringify(bad), { status: 200 }));
      expect((await previewPluginUpdate("acme.plugin")).kind).toBe("error");
    }
  });
});

describe("applyPluginUpdate", () => {
  it("POSTs the fingerprint and returns a job id on success", async () => {
    fetchSpy.mockResolvedValue(new Response(JSON.stringify({ job_id: "job1" }), { status: 202 }));
    const res = await applyPluginUpdate("acme.plugin", "treeB||community");
    expect(res).toEqual({ kind: "ok", jobId: "job1" });
    const [url, init] = fetchSpy.mock.calls[0];
    expect(url).toBe("/api/plugins/acme.plugin/update/apply");
    expect(init?.method).toBe("POST");
    expect(JSON.parse(String(init?.body))).toEqual({ expected_fingerprint: "treeB||community" });
  });

  it("surfaces a conflict message (moved remote)", async () => {
    fetchSpy.mockResolvedValue(new Response(JSON.stringify({ message: "changed since shown" }), { status: 409 }));
    expect(await applyPluginUpdate("acme.plugin", "x")).toEqual({ kind: "error", message: "changed since shown" });
  });

  it("reports an error when an OK response carries no job id", async () => {
    fetchSpy.mockResolvedValue(new Response(JSON.stringify({ nope: true }), { status: 200 }));
    expect((await applyPluginUpdate("acme.plugin", null)).kind).toBe("error");
  });

  it("returns a network error when the request throws", async () => {
    fetchSpy.mockRejectedValue(new Error("offline"));
    expect(await applyPluginUpdate("acme.plugin", null)).toEqual({ kind: "error", message: "Network error." });
  });
});

describe("dismissPluginUpdate", () => {
  it("POSTs the fingerprint and returns ok on success", async () => {
    fetchSpy.mockResolvedValue(new Response("", { status: 200 }));
    const res = await dismissPluginUpdate("acme.plugin", "treeB||community");
    expect(res).toEqual({ kind: "ok" });
    const [url, init] = fetchSpy.mock.calls[0];
    expect(url).toBe("/api/plugins/acme.plugin/update/dismiss");
    expect(init?.method).toBe("POST");
    expect(JSON.parse(String(init?.body))).toEqual({ fingerprint: "treeB||community" });
  });

  it("surfaces the server message on a non-OK response", async () => {
    fetchSpy.mockResolvedValue(new Response(JSON.stringify({ message: "read-only" }), { status: 403 }));
    expect(await dismissPluginUpdate("acme.plugin", "x")).toEqual({ kind: "error", message: "read-only" });
  });

  it("returns a network error when the request throws", async () => {
    fetchSpy.mockRejectedValue(new Error("offline"));
    expect(await dismissPluginUpdate("acme.plugin", "x")).toEqual({ kind: "error", message: "Network error." });
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

describe("resolvePluginOptions", () => {
  it("POSTs the source/depends and returns the resolved options (#2897)", async () => {
    const options = [{ value: "claude-code", label: "Claude Code" }];
    fetchSpy.mockResolvedValue(new Response(JSON.stringify({ options }), { status: 200 }));
    expect(await resolvePluginOptions("acme.cron", "acp_agents", ["x"])).toEqual(options);
    const [url, init] = fetchSpy.mock.calls[0];
    expect(url).toBe("/api/plugins/acme.cron/settings/options/resolve");
    expect(init?.method).toBe("POST");
    expect(JSON.parse(String(init?.body))).toEqual({ source: "acp_agents", depends: ["x"] });
  });

  it("returns [] on a non-OK response, a missing options field, and a network throw", async () => {
    fetchSpy.mockResolvedValue(new Response("nope", { status: 500 }));
    expect(await resolvePluginOptions("acme.cron", "acp_agents", [])).toEqual([]);
    fetchSpy.mockResolvedValue(new Response(JSON.stringify({}), { status: 200 }));
    expect(await resolvePluginOptions("acme.cron", "acp_agents", [])).toEqual([]);
    fetchSpy.mockRejectedValue(new Error("offline"));
    expect(await resolvePluginOptions("acme.cron", "acp_agents", [])).toEqual([]);
  });
});
