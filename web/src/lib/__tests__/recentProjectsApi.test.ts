// Vitest coverage for the recent-projects API client (#2141): the wizard
// fetches GET /api/recent-projects and folds it into the Recent list. Like the
// other read helpers it swallows network and non-OK responses (returns null)
// so a failure degrades to the session-derived list rather than blocking.

import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import { fetchRecentProjects } from "../api";

const fetchSpy = vi.fn<typeof fetch>();

beforeEach(() => {
  fetchSpy.mockReset();
  vi.stubGlobal("fetch", fetchSpy);
});

afterEach(() => {
  vi.unstubAllGlobals();
});

describe("fetchRecentProjects (#2141)", () => {
  it("GETs the endpoint and returns the parsed envelope", async () => {
    const payload = {
      projects: [
        { path: "/repo/frontend", display_name: "frontend", tool: "claude", last_used_at: "2025-09-09T00:00:00+00:00" },
      ],
    };
    fetchSpy.mockResolvedValue(new Response(JSON.stringify(payload), { status: 200 }));

    const result = await fetchRecentProjects();

    expect(result).toEqual(payload);
    expect(fetchSpy.mock.calls[0][0]).toBe("/api/recent-projects");
  });

  it("returns null on a non-OK response", async () => {
    fetchSpy.mockResolvedValue(new Response("nope", { status: 500 }));
    expect(await fetchRecentProjects()).toBeNull();
  });

  it("returns null when the request throws", async () => {
    fetchSpy.mockRejectedValue(new Error("network down"));
    expect(await fetchRecentProjects()).toBeNull();
  });
});
