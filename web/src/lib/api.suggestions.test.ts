import { afterEach, describe, expect, it, vi } from "vitest";
import {
  actOnSuggestion,
  fetchSuggestions,
  prepareSuggestion,
  refreshSuggestions,
  updateSuggestionPolicy,
  updateSuggestionSource,
} from "./api";

afterEach(() => vi.unstubAllGlobals());

describe("suggestion API", () => {
  it("scopes read, refresh, prepare, actions, and policy to the selected profile", async () => {
    const fetchMock = vi
      .fn()
      .mockResolvedValueOnce(
        new Response(
          JSON.stringify({ items: [], providers: [], auto_create: { enabled: false, rules: [], last_error: null } }),
        ),
      )
      .mockResolvedValueOnce(new Response(null, { status: 204 }))
      .mockResolvedValueOnce(
        new Response(
          JSON.stringify({
            project_path: "/repo",
            title: "Draft",
            prompt: "Review",
            source_ref: "https://example.test/1",
            suggestion: { provider_id: "gh", id: "1", revision: 3 },
          }),
        ),
      )
      .mockResolvedValueOnce(new Response(JSON.stringify({ session_id: "session-1", created: true })))
      .mockResolvedValueOnce(new Response(null, { status: 204 }));
    vi.stubGlobal("fetch", fetchMock);
    const identity = { provider_id: "gh", id: "1", revision: 3 };

    expect(await fetchSuggestions("profile one")).toMatchObject({ items: [] });
    expect(await refreshSuggestions("profile one")).toBe(true);
    expect(await prepareSuggestion(identity, "profile one")).toMatchObject({
      ok: true,
      data: { project_path: "/repo" },
    });
    expect(await actOnSuggestion(identity, "start", "profile one")).toMatchObject({
      ok: true,
      data: { session_id: "session-1" },
    });
    expect(await updateSuggestionPolicy(true, [{ provider_id: "gh", criterion: "assigned" }], "profile one")).toBe(
      true,
    );

    expect(fetchMock.mock.calls.map(([url, init]) => [url, init?.method])).toEqual([
      ["/api/suggestions?profile=profile%20one", undefined],
      ["/api/suggestions/refresh?profile=profile%20one", "POST"],
      ["/api/suggestions/prepare?profile=profile%20one", "POST"],
      ["/api/suggestions/action?profile=profile%20one", "POST"],
      ["/api/suggestions/policy?profile=profile%20one", "PATCH"],
    ]);
    expect(JSON.parse(fetchMock.mock.calls[3][1].body as string)).toMatchObject({ ...identity, action: "start" });
    expect(JSON.parse(fetchMock.mock.calls[4][1].body as string)).toEqual({
      enabled: true,
      rules: [{ provider_id: "gh", criterion: "assigned" }],
    });
  });

  it("returns the network error for start and preparation failures", async () => {
    const fetchMock = vi.fn().mockRejectedValue(new Error("offline"));
    vi.stubGlobal("fetch", fetchMock);
    const identity = { provider_id: "gh", id: "1", revision: 3 };

    expect(await actOnSuggestion(identity, "start")).toEqual({ ok: false, error: "offline" });
    expect(await prepareSuggestion(identity)).toEqual({ ok: false, error: "offline" });
    expect(fetchMock).toHaveBeenCalledTimes(2);
  });

  it("returns refresh failures instead of claiming success", async () => {
    vi.stubGlobal("fetch", vi.fn().mockResolvedValue(new Response("", { status: 503 })));
    expect(await refreshSuggestions()).toBe(false);
  });

  it("returns server messages and status fallbacks for rejected requests", async () => {
    const fetchMock = vi
      .fn()
      .mockResolvedValueOnce(new Response(JSON.stringify({ message: "Start denied" }), { status: 409 }))
      .mockResolvedValueOnce(new Response("{}", { status: 503 }));
    vi.stubGlobal("fetch", fetchMock);
    const identity = { provider_id: "gh", id: "1", revision: 3 };

    expect(await actOnSuggestion(identity, "start")).toEqual({ ok: false, error: "Start denied" });
    expect(await prepareSuggestion(identity)).toEqual({ ok: false, error: "Request failed (503)" });
  });

  it("switches one provider source for the selected profile", async () => {
    const fetchMock = vi.fn().mockResolvedValueOnce(new Response(JSON.stringify({ enabled: false })));
    vi.stubGlobal("fetch", fetchMock);
    expect(await updateSuggestionSource("gh", "ci_failed", false, "profile one")).toBe(true);
    const [url, init] = fetchMock.mock.calls[0] as [string, RequestInit];
    expect(url).toBe("/api/suggestions/sources?profile=profile%20one");
    expect(init.method).toBe("PATCH");
    expect(JSON.parse(String(init.body))).toEqual({ provider_id: "gh", criterion: "ci_failed", enabled: false });
  });
});
