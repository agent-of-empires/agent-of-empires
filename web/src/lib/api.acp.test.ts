// @vitest-environment jsdom
//
// Wire-shape contract for the structured view ACP registry + switch-agent
// helpers added for the rate-limit recovery flow (#1281 / #1282).
// Pins the URL, method, headers, JSON body, and the empty-array
// fallback for the agents fetch (fetchJson returns null on
// 4xx/5xx and the helper must coalesce to []).

import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import {
  acpDisable,
  acpEnable,
  fetchAcpAgents,
  installAcpAgent,
  switchAcpAgent,
  type SwitchAgentResponse,
} from "./api";

const originalFetch = globalThis.fetch;

beforeEach(() => {
  // Default to a 404-returning fetch so any unexpected URL surfaces as
  // null from fetchJson rather than a hung test.
  globalThis.fetch = vi
    .fn()
    .mockResolvedValue(new Response("not found", { status: 404 })) as unknown as typeof globalThis.fetch;
});

afterEach(() => {
  globalThis.fetch = originalFetch;
});

function ok(body: unknown): Response {
  return new Response(JSON.stringify(body), {
    status: 200,
    headers: { "content-type": "application/json" },
  });
}

describe("fetchAcpAgents", () => {
  it("returns the array from the /api/acp/agents response", async () => {
    const agents = [
      { name: "claude", description: "Claude", command: "claude-agent-acp" },
      { name: "codex", description: "OpenAI Codex", command: "codex-acp" },
    ];
    (globalThis.fetch as ReturnType<typeof vi.fn>).mockResolvedValueOnce(ok(agents));
    const result = await fetchAcpAgents();
    expect(result).toEqual(agents);
    const url = (globalThis.fetch as ReturnType<typeof vi.fn>).mock.calls[0]?.[0];
    expect(String(url)).toContain("/api/acp/agents");
  });

  it("coalesces a null fetchJson result to []", async () => {
    // 4xx -> fetchJson returns null -> helper returns [].
    (globalThis.fetch as ReturnType<typeof vi.fn>).mockResolvedValueOnce(new Response("nope", { status: 404 }));
    const result = await fetchAcpAgents();
    expect(result).toEqual([]);
  });
});

describe("switchAcpAgent", () => {
  it("POSTs the target to /api/sessions/:id/acp/switch-agent", async () => {
    const response: SwitchAgentResponse = {
      session_id: "s-1",
      agent: "codex",
      before_seq: 41,
      switch_seq: 42,
      status: "switched",
    };
    (globalThis.fetch as ReturnType<typeof vi.fn>).mockResolvedValueOnce(ok(response));
    const result = await switchAcpAgent("s-1", "codex");
    expect(result).toEqual(response);
    const [url, init] = (globalThis.fetch as ReturnType<typeof vi.fn>).mock.calls[0];
    expect(String(url)).toContain("/api/sessions/s-1/acp/switch-agent");
    const req = init as RequestInit;
    expect(req.method).toBe("POST");
    expect(JSON.parse(req.body as string)).toEqual({ target: "codex" });
  });

  it("encodes the session id in the URL path", async () => {
    (globalThis.fetch as ReturnType<typeof vi.fn>).mockResolvedValueOnce(
      ok({
        session_id: "weird/id",
        agent: "codex",
        before_seq: 0,
        switch_seq: 1,
        status: "ok",
      }),
    );
    await switchAcpAgent("weird/id", "codex");
    const url = (globalThis.fetch as ReturnType<typeof vi.fn>).mock.calls[0]?.[0];
    expect(String(url)).toContain("weird%2Fid");
  });

  it("includes the model field only when provided", async () => {
    (globalThis.fetch as ReturnType<typeof vi.fn>).mockResolvedValueOnce(
      ok({
        session_id: "s-1",
        agent: "codex",
        before_seq: 0,
        switch_seq: 1,
        status: "ok",
      }),
    );
    await switchAcpAgent("s-1", "codex", "opus-4.7");
    const [, init] = (globalThis.fetch as ReturnType<typeof vi.fn>).mock.calls[0];
    const body = JSON.parse((init as RequestInit).body as string);
    expect(body).toEqual({ target: "codex", model: "opus-4.7" });
  });

  it("includes the reason field only when provided", async () => {
    (globalThis.fetch as ReturnType<typeof vi.fn>).mockResolvedValueOnce(
      ok({
        session_id: "s-1",
        agent: "claude",
        before_seq: 0,
        switch_seq: 1,
        status: "ok",
      }),
    );
    await switchAcpAgent("s-1", "claude", null, "manual");
    const [, init] = (globalThis.fetch as ReturnType<typeof vi.fn>).mock.calls[0];
    const body = JSON.parse((init as RequestInit).body as string);
    expect(body).toEqual({ target: "claude", reason: "manual" });
  });

  it("returns null when fetchJson reports a non-2xx", async () => {
    (globalThis.fetch as ReturnType<typeof vi.fn>).mockResolvedValueOnce(new Response("conflict", { status: 409 }));
    const result = await switchAcpAgent("s-1", "codex");
    expect(result).toBeNull();
  });
});

describe("installAcpAgent", () => {
  it("POSTs to /api/sessions/:id/acp/install-agent and returns the parsed body", async () => {
    const body = {
      session_id: "s-1",
      package: "@agentclientprotocol/codex-acp@latest",
      success: true,
      exit_code: 0,
      stdout: "added 1 package",
      stderr: "",
    };
    (globalThis.fetch as ReturnType<typeof vi.fn>).mockResolvedValueOnce(ok(body));
    const result = await installAcpAgent("s-1");
    expect(result).toEqual(body);
    const [url, init] = (globalThis.fetch as ReturnType<typeof vi.fn>).mock.calls[0];
    expect(String(url)).toContain("/api/sessions/s-1/acp/install-agent");
    expect((init as RequestInit).method).toBe("POST");
  });

  it("encodes the session id in the URL path", async () => {
    (globalThis.fetch as ReturnType<typeof vi.fn>).mockResolvedValueOnce(
      ok({ session_id: "weird/id", package: "p", success: true, exit_code: 0, stdout: "", stderr: "" }),
    );
    await installAcpAgent("weird/id");
    const url = (globalThis.fetch as ReturnType<typeof vi.fn>).mock.calls[0]?.[0];
    expect(String(url)).toContain("weird%2Fid");
  });

  it("throws the server message on a non-2xx response", async () => {
    (globalThis.fetch as ReturnType<typeof vi.fn>).mockResolvedValueOnce(
      new Response(JSON.stringify({ error: "install_disabled", message: "Installing agents from the web is off." }), {
        status: 403,
        headers: { "content-type": "application/json" },
      }),
    );
    await expect(installAcpAgent("s-1")).rejects.toThrow("Installing agents from the web is off.");
  });

  it("falls back to the status code when the error body has no message", async () => {
    (globalThis.fetch as ReturnType<typeof vi.fn>).mockResolvedValueOnce(new Response("boom", { status: 500 }));
    await expect(installAcpAgent("s-1")).rejects.toThrow("Server returned 500");
  });

  it("throws on a 2xx response with an empty/invalid body", async () => {
    // 200 but unparseable JSON -> body is null -> must throw, not return null.
    (globalThis.fetch as ReturnType<typeof vi.fn>).mockResolvedValueOnce(
      new Response("not json", { status: 200, headers: { "content-type": "application/json" } }),
    );
    await expect(installAcpAgent("s-1")).rejects.toThrow("invalid or empty response");
  });
});

describe("acpEnable / acpDisable", () => {
  it("acpEnable POSTs to /acp/enable and returns the view", async () => {
    (globalThis.fetch as ReturnType<typeof vi.fn>).mockResolvedValueOnce(ok({ session_id: "s-1", view: "structured" }));
    const result = await acpEnable("s-1");
    expect(result).toEqual({ session_id: "s-1", view: "structured" });
    const [url, init] = (globalThis.fetch as ReturnType<typeof vi.fn>).mock.calls[0];
    expect(String(url)).toContain("/api/sessions/s-1/acp/enable");
    expect((init as RequestInit).method).toBe("POST");
  });

  it("acpDisable POSTs to /acp/disable and returns the view", async () => {
    (globalThis.fetch as ReturnType<typeof vi.fn>).mockResolvedValueOnce(ok({ session_id: "s-1", view: "terminal" }));
    const result = await acpDisable("s-1");
    expect(result).toEqual({ session_id: "s-1", view: "terminal" });
    const [url, init] = (globalThis.fetch as ReturnType<typeof vi.fn>).mock.calls[0];
    expect(String(url)).toContain("/api/sessions/s-1/acp/disable");
    expect((init as RequestInit).method).toBe("POST");
  });

  it("encodes the session id and returns null on non-2xx", async () => {
    (globalThis.fetch as ReturnType<typeof vi.fn>).mockResolvedValueOnce(ok({ session_id: "a/b", view: "terminal" }));
    await acpDisable("a/b");
    const url = (globalThis.fetch as ReturnType<typeof vi.fn>).mock.calls[0]?.[0];
    expect(String(url)).toContain("/api/sessions/a%2Fb/acp/disable");
    // default mocked fetch is 404 -> fetchJson returns null.
    expect(await acpEnable("missing")).toBeNull();
  });
});
