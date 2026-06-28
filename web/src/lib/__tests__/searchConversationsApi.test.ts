// Vitest coverage for the conversation-search API client (#2515). The palette
// hook GETs /api/sessions/search and folds the results into a Conversations
// group. Like the other read helpers it degrades to an empty list on a
// non-OK or failed request rather than throwing.

import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import { searchConversations } from "../api";

const fetchSpy = vi.fn<typeof fetch>();

beforeEach(() => {
  fetchSpy.mockReset();
  vi.stubGlobal("fetch", fetchSpy);
});

afterEach(() => {
  vi.unstubAllGlobals();
});

describe("searchConversations (#2515)", () => {
  it("GETs the search endpoint with the encoded query and returns results", async () => {
    const results = [{ session_id: "s1", seq: 3, kind: "agent", snippet: "hit", match_count: 2 }];
    fetchSpy.mockResolvedValue(new Response(JSON.stringify({ results }), { status: 200 }));

    const out = await searchConversations("foo bar");

    expect(out).toEqual(results);
    expect(fetchSpy.mock.calls[0][0]).toBe("/api/sessions/search?q=foo%20bar");
  });

  it("forwards an abort signal to fetch", async () => {
    fetchSpy.mockResolvedValue(new Response(JSON.stringify({ results: [] }), { status: 200 }));
    const controller = new AbortController();
    await searchConversations("q", controller.signal);
    expect((fetchSpy.mock.calls[0][1] as RequestInit).signal).toBe(controller.signal);
  });

  it("returns an empty list on a non-OK response", async () => {
    fetchSpy.mockResolvedValue(new Response("nope", { status: 500 }));
    expect(await searchConversations("q")).toEqual([]);
  });
});
