import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import {
  clearServerQueue,
  editServerQueuedPrompt,
  enqueueServerPrompt,
  listServerQueue,
  removeServerQueuedPrompt,
} from "../api";

const fetchSpy = vi.fn<typeof fetch>();

beforeEach(() => {
  fetchSpy.mockReset();
  vi.stubGlobal("fetch", fetchSpy);
});

afterEach(() => {
  vi.unstubAllGlobals();
});

function lastCall() {
  const [url, init] = fetchSpy.mock.calls.at(-1)!;
  return { url: String(url), init: init ?? {} };
}

describe("server queue API client", () => {
  it("enqueue POSTs the client-minted id + text and returns the stored entry", async () => {
    const entry = { id: "q1", seq: 3, text: "hi", created_at: "t0" };
    fetchSpy.mockResolvedValue(new Response(JSON.stringify(entry), { status: 200 }));

    const result = await enqueueServerPrompt("s1", { id: "q1", text: "hi", createdAt: "t0" });

    expect(result).toEqual(entry);
    const { url, init } = lastCall();
    expect(url).toBe("/api/sessions/s1/queue");
    expect(init.method).toBe("POST");
    expect(JSON.parse(String(init.body))).toMatchObject({ id: "q1", text: "hi", created_at: "t0" });
  });

  it("enqueue returns null on a non-2xx (so the caller keeps the optimistic row to retry)", async () => {
    fetchSpy.mockResolvedValue(new Response("nope", { status: 503 }));
    expect(await enqueueServerPrompt("s1", { id: "q1", text: "hi" })).toBeNull();
  });

  it("list returns the array, or [] on failure", async () => {
    const rows = [{ id: "a", seq: 0, text: "one", created_at: "t" }];
    fetchSpy.mockResolvedValueOnce(new Response(JSON.stringify(rows), { status: 200 }));
    expect(await listServerQueue("s1")).toEqual(rows);

    fetchSpy.mockResolvedValueOnce(new Response("boom", { status: 500 }));
    expect(await listServerQueue("s1")).toEqual([]);
  });

  it("edit PATCHes the text and reports ok", async () => {
    fetchSpy.mockResolvedValue(new Response(null, { status: 204 }));
    expect(await editServerQueuedPrompt("s1", "q1", "edited")).toBe(true);
    const { url, init } = lastCall();
    expect(url).toBe("/api/sessions/s1/queue/q1");
    expect(init.method).toBe("PATCH");
    expect(JSON.parse(String(init.body))).toEqual({ text: "edited" });
  });

  it("edit reports false on a 404 (row already drained/removed)", async () => {
    fetchSpy.mockResolvedValue(new Response("gone", { status: 404 }));
    expect(await editServerQueuedPrompt("s1", "q1", "edited")).toBe(false);
  });

  it("remove DELETEs the row and clear DELETEs the whole queue", async () => {
    fetchSpy.mockResolvedValue(new Response(null, { status: 204 }));
    expect(await removeServerQueuedPrompt("s1", "q1")).toBe(true);
    expect(lastCall().url).toBe("/api/sessions/s1/queue/q1");
    expect(lastCall().init.method).toBe("DELETE");

    expect(await clearServerQueue("s1")).toBe(true);
    expect(lastCall().url).toBe("/api/sessions/s1/queue");
    expect(lastCall().init.method).toBe("DELETE");
  });

  it("encodes session and prompt ids into the path", async () => {
    fetchSpy.mockResolvedValue(new Response(null, { status: 204 }));
    await removeServerQueuedPrompt("a/b", "c d");
    expect(lastCall().url).toBe("/api/sessions/a%2Fb/queue/c%20d");
  });

  it("a fetch throw is swallowed (null / false, never a rejection)", async () => {
    fetchSpy.mockRejectedValue(new Error("network"));
    expect(await enqueueServerPrompt("s1", { id: "q1", text: "hi" })).toBeNull();
    expect(await clearServerQueue("s1")).toBe(false);
  });
});
