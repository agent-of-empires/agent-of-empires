import { afterEach, describe, expect, it, vi } from "vitest";
import {
  dismissSuggestion,
  fetchSuggestions,
  prepareSuggestion,
  refreshSuggestions,
  updateSuggestionSource,
} from "./api";

afterEach(() => vi.unstubAllGlobals());

describe("suggestion API", () => {
  it("sends the documented requests", async () => {
    const fetchMock = vi.fn(async () => new Response(JSON.stringify({ providers: [], items: [] })));
    vi.stubGlobal("fetch", fetchMock);

    await fetchSuggestions();
    await refreshSuggestions();
    await dismissSuggestion("gh", "1", true);
    await dismissSuggestion("gh", "1", false);
    await updateSuggestionSource("gh", "assigned", false);
    await prepareSuggestion("gh", "1");

    const calls = fetchMock.mock.calls.map(([url, init]) => [url, init?.method ?? "GET", init?.body]);
    expect(calls).toEqual([
      ["/api/suggestions", "GET", undefined],
      ["/api/suggestions/refresh", "POST", undefined],
      ["/api/suggestions/dismiss", "POST", JSON.stringify({ provider_id: "gh", id: "1" })],
      ["/api/suggestions/undismiss", "POST", JSON.stringify({ provider_id: "gh", id: "1" })],
      [
        "/api/suggestions/sources",
        "PATCH",
        JSON.stringify({ provider_id: "gh", criterion: "assigned", enabled: false }),
      ],
      ["/api/suggestions/prepare", "POST", JSON.stringify({ provider_id: "gh", id: "1" })],
    ]);
  });

  it("flags a 409 as stale and surfaces the server message", async () => {
    vi.stubGlobal(
      "fetch",
      vi.fn(
        async () => new Response(JSON.stringify({ message: "Suggestion is no longer published" }), { status: 409 }),
      ),
    );
    expect(await prepareSuggestion("gh", "1")).toEqual({
      ok: false,
      error: "Suggestion is no longer published",
      stale: true,
    });
  });
});
