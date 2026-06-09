// @vitest-environment jsdom
//
// Wire-shape contract for the structured view ACP registry + switch-agent
// helpers added for the rate-limit recovery flow (#1281 / #1282).
// Pins the URL, method, headers, JSON body, and the empty-array
// fallback for the agents fetch (fetchJson returns null on
// 4xx/5xx and the helper must coalesce to []).

import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import { fetchAcpAgents, switchAcpAgent, type SwitchAgentResponse } from "./api";

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
