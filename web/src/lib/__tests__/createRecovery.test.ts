// @vitest-environment jsdom
// Recovery through the real create client: only the network is stubbed, so a proxy
// timeout page reaches the owner exactly as the browser would deliver it.

import { afterEach, describe, expect, it, vi } from "vitest";
import { registerPendingCreate, startPendingCreates } from "../pendingCreates";

afterEach(() => {
  vi.unstubAllGlobals();
  localStorage.clear();
});

describe("create recovery through a reverse proxy", () => {
  it("keeps a create answered by a 504 page and completes it under its original key", async () => {
    const keys: string[] = [];
    const replies = [
      new Response("<html><h1>504 Gateway Time-out</h1></html>", { status: 504 }),
      new Response(JSON.stringify({ id: "s1", title: "created" }), { status: 201 }),
    ];
    vi.stubGlobal(
      "fetch",
      vi.fn(async (_url: string, init?: RequestInit) => {
        keys.push(JSON.parse(String(init?.body)).idempotency_key);
        return replies.shift()!;
      }),
    );
    const onCreated = vi.fn();
    const onFailed = vi.fn();
    startPendingCreates({ onCreated, onFailed });
    registerPendingCreate({
      body: { path: "/tmp/p", tool: "claude", idempotency_key: "k-504" },
      tool: "claude",
      since: Date.now(),
    });

    await vi.waitFor(() => expect(onCreated).toHaveBeenCalled(), { timeout: 5000 });
    expect(onFailed).not.toHaveBeenCalled();
    expect(keys).toEqual(["k-504", "k-504"]);
  });
});
