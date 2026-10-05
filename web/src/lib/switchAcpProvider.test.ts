// A failed provider switch can leave the session changed, so the reason the
// server gave is the only thing that tells the user what state they are in.

import { afterEach, describe, expect, it, vi } from "vitest";

import { switchAcpProvider } from "./api";

function stubFetch(status: number, body: string) {
  vi.stubGlobal("fetch", vi.fn().mockResolvedValue(new Response(body, { status })));
}

afterEach(() => {
  vi.unstubAllGlobals();
});

describe("switchAcpProvider", () => {
  it("returns the parsed response when the switch lands", async () => {
    stubFetch(200, JSON.stringify({ session_id: "s1", provider: "vertex", model_cleared: true, status: "running" }));
    await expect(switchAcpProvider("s1", "vertex")).resolves.toMatchObject({
      provider: "vertex",
      model_cleared: true,
    });
  });

  // A refusal is structured; a respawn that failed after the worker stopped
  // answers plain text, and an empty body leaves only the status.
  it.each([
    [
      409,
      JSON.stringify({ error: "provider_switch_unsupported", message: "this session runs codex" }),
      "this session runs codex",
    ],
    [500, "shutdown failed before provider switch: timed out", "shutdown failed before provider switch: timed out"],
    [500, "", "request failed (500)"],
  ])("surfaces the server's reason on %i", async (status, body, expected) => {
    stubFetch(status, body);
    await expect(switchAcpProvider("s1", "vertex")).rejects.toThrow(expected);
  });
});
