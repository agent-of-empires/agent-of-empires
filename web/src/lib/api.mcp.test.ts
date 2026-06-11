// Contract test for the MCP api-client wrappers (#1996). These mutation
// helpers are otherwise only exercised by the live Playwright panel, whose
// coverage does not feed the Vitest patch lane, so the status-to-result
// mapping (`applied` / `stale` / `error`), the request payloads, and the
// network-failure fallbacks are locked in here at the api-client layer.

import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import { fetchMcpServers, resolveMcpConflict, keepMcpServer, dropMcpServer, type McpServersResponse } from "./api";

function jsonResponse(body: unknown, status = 200): Response {
  return new Response(JSON.stringify(body), {
    status,
    headers: { "content-type": "application/json" },
  });
}

function makeResponse(overrides: Partial<McpServersResponse> = {}): McpServersResponse {
  return {
    agent: "claude",
    effective: [],
    keptOnRemoval: [],
    conflicts: [],
    driftPaused: false,
    ...overrides,
  };
}

const fetchSpy = vi.fn<typeof fetch>();

beforeEach(() => {
  fetchSpy.mockReset();
  vi.stubGlobal("fetch", fetchSpy);
});

afterEach(() => {
  vi.unstubAllGlobals();
});

describe("fetchMcpServers", () => {
  it("returns the parsed payload and omits the query when no agent is given", async () => {
    fetchSpy.mockResolvedValueOnce(jsonResponse(makeResponse({ agent: "claude" })));
    const res = await fetchMcpServers();
    expect(res?.agent).toBe("claude");
    expect(fetchSpy).toHaveBeenCalledWith("/api/mcp/servers", undefined);
  });

  it("encodes the agent into the query string", async () => {
    fetchSpy.mockResolvedValueOnce(jsonResponse(makeResponse()));
    await fetchMcpServers("claude code");
    expect(fetchSpy).toHaveBeenCalledWith("/api/mcp/servers?agent=claude%20code", undefined);
  });

  it("returns null on non-2xx and on network failure", async () => {
    fetchSpy.mockResolvedValueOnce(new Response("", { status: 500 }));
    expect(await fetchMcpServers()).toBeNull();
    fetchSpy.mockRejectedValueOnce(new Error("offline"));
    expect(await fetchMcpServers()).toBeNull();
  });
});

describe("resolveMcpConflict", () => {
  it("POSTs the winner + fingerprint and maps 2xx to `applied`", async () => {
    fetchSpy.mockResolvedValueOnce(jsonResponse({ status: "applied" }));
    const result = await resolveMcpConflict("fs", "claude", "aoe", "fp-123");
    expect(result).toBe("applied");
    const [url, init] = fetchSpy.mock.calls[0]!;
    expect(url).toBe("/api/mcp/servers/fs/resolve");
    expect(init?.method).toBe("POST");
    expect(JSON.parse(init!.body as string)).toEqual({
      agent: "claude",
      winner: "aoe",
      fingerprint: "fp-123",
    });
  });

  it("maps 409 to `stale` and other non-2xx to `error`", async () => {
    fetchSpy.mockResolvedValueOnce(new Response("", { status: 409 }));
    expect(await resolveMcpConflict("fs", "claude", "native", "fp")).toBe("stale");
    fetchSpy.mockResolvedValueOnce(new Response("", { status: 500 }));
    expect(await resolveMcpConflict("fs", "claude", "native", "fp")).toBe("error");
  });

  it("maps a network failure to `error`", async () => {
    fetchSpy.mockRejectedValueOnce(new Error("offline"));
    expect(await resolveMcpConflict("fs", "claude", "aoe", "fp")).toBe("error");
  });

  it("percent-encodes the server name in the path", async () => {
    fetchSpy.mockResolvedValueOnce(jsonResponse({ status: "applied" }));
    await resolveMcpConflict("a/b", "claude", "aoe", "fp");
    expect(fetchSpy.mock.calls[0]![0]).toBe("/api/mcp/servers/a%2Fb/resolve");
  });
});

describe("keepMcpServer / dropMcpServer", () => {
  it("keep POSTs the agent and returns true on 2xx", async () => {
    fetchSpy.mockResolvedValueOnce(jsonResponse({ status: "kept" }));
    expect(await keepMcpServer("fs", "claude")).toBe(true);
    const [url, init] = fetchSpy.mock.calls[0]!;
    expect(url).toBe("/api/mcp/servers/fs/keep");
    expect(init?.method).toBe("POST");
    expect(JSON.parse(init!.body as string)).toEqual({ agent: "claude" });
  });

  it("drop POSTs the agent and returns true on 2xx", async () => {
    fetchSpy.mockResolvedValueOnce(jsonResponse({ status: "dropped" }));
    expect(await dropMcpServer("fs", "claude")).toBe(true);
    expect(fetchSpy.mock.calls[0]![0]).toBe("/api/mcp/servers/fs/drop");
    expect(JSON.parse(fetchSpy.mock.calls[0]![1]!.body as string)).toEqual({
      agent: "claude",
    });
  });

  it("both return false on a non-2xx response and on a network failure", async () => {
    fetchSpy.mockResolvedValueOnce(new Response("", { status: 403 }));
    expect(await keepMcpServer("fs", "claude")).toBe(false);
    fetchSpy.mockRejectedValueOnce(new Error("offline"));
    expect(await keepMcpServer("fs", "claude")).toBe(false);

    fetchSpy.mockResolvedValueOnce(new Response("", { status: 403 }));
    expect(await dropMcpServer("fs", "claude")).toBe(false);
    fetchSpy.mockRejectedValueOnce(new Error("offline"));
    expect(await dropMcpServer("fs", "claude")).toBe(false);
  });
});
