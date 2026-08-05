import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import {
  adoptSkill,
  createSkill,
  deleteSkill,
  fetchSkill,
  fetchSkills,
  syncSkills,
  updateSkill,
  type SkillsResponse,
} from "./api";

function jsonResponse(body: unknown, status = 200): Response {
  return new Response(JSON.stringify(body), {
    status,
    headers: { "content-type": "application/json" },
  });
}

const fetchSpy = vi.fn<typeof fetch>();

beforeEach(() => {
  fetchSpy.mockReset();
  vi.stubGlobal("fetch", fetchSpy);
});

afterEach(() => {
  vi.unstubAllGlobals();
});

describe("skills API", () => {
  it("reads source-qualified skills and sends each managed mutation contract", async () => {
    const list: SkillsResponse = { skills: [], roots: [] };
    fetchSpy.mockResolvedValueOnce(jsonResponse(list));
    expect(await fetchSkills()).toEqual(list);
    expect(fetchSpy.mock.calls[0]).toEqual(["/api/skills", undefined]);

    const detail = {
      directory: "review",
      name: "review",
      description: "Review code",
      provenance: { kind: "external" as const, root: "claude-user" },
      content: "---\nname: review\ndescription: Review code\n---\n",
    };
    fetchSpy.mockResolvedValueOnce(jsonResponse(detail));
    expect(await fetchSkill("claude user", "review/a")).toEqual(detail);
    expect(fetchSpy.mock.calls[1]).toEqual(["/api/skills/claude%20user/review%2Fa", undefined]);

    fetchSpy.mockResolvedValueOnce(jsonResponse({ ok: true, directory: "mine" }, 201));
    expect(await createSkill("mine", "Mine")).toMatchObject({ ok: true, directory: "mine" });
    expect(fetchSpy.mock.calls[2]?.[0]).toBe("/api/skills");
    expect(JSON.parse(fetchSpy.mock.calls[2]?.[1]?.body as string)).toEqual({
      directory: "mine",
      description: "Mine",
    });

    fetchSpy.mockResolvedValueOnce(jsonResponse({ ok: true }));
    expect(await updateSkill("mine", "content")).toMatchObject({ ok: true });
    expect(fetchSpy.mock.calls[3]?.[1]?.method).toBe("PUT");
    expect(JSON.parse(fetchSpy.mock.calls[3]?.[1]?.body as string)).toEqual({ content: "content" });

    fetchSpy.mockResolvedValueOnce(jsonResponse({ ok: true, directory: "adopted" }, 201));
    expect(await adoptSkill("claude-user", "review", "adopted")).toMatchObject({
      ok: true,
      directory: "adopted",
    });
    expect(fetchSpy.mock.calls[4]?.[0]).toBe("/api/skills/claude-user/review/adopt");
    expect(JSON.parse(fetchSpy.mock.calls[4]?.[1]?.body as string)).toEqual({ destination: "adopted" });

    fetchSpy.mockResolvedValueOnce(jsonResponse({ ok: true }));
    expect(await deleteSkill("mine")).toMatchObject({ ok: true });
    expect(fetchSpy.mock.calls[5]?.[1]?.method).toBe("DELETE");
  });

  it("returns null for failed reads and preserves mutation error messages", async () => {
    fetchSpy.mockResolvedValueOnce(new Response("", { status: 500 }));
    expect(await fetchSkills()).toBeNull();
    fetchSpy.mockRejectedValueOnce(new Error("offline"));
    expect(await fetchSkill("aoe-managed", "mine")).toBeNull();

    fetchSpy.mockResolvedValueOnce(jsonResponse({ message: "already exists" }, 409));
    expect(await createSkill("mine")).toEqual({
      ok: false,
      error: "already exists",
      status: 409,
    });
    fetchSpy.mockRejectedValueOnce(new Error("offline"));
    expect(await deleteSkill("mine")).toEqual({
      ok: false,
      error: "Network error: offline",
    });
  });

  it("posts the right body for syncSkills across every options shape and preserves an error message on failure", async () => {
    const cases: {
      label: string;
      options: { roots?: string[]; replace?: string[]; directories?: string[] } | undefined;
      expectedBody: Record<string, unknown>;
    }[] = [
      { label: "no options", options: undefined, expectedBody: {} },
      {
        label: "roots only",
        options: { roots: ["claude-user", "aoe-managed"] },
        expectedBody: { roots: ["claude-user", "aoe-managed"] },
      },
      { label: "replace only", options: { replace: ["aoe-review"] }, expectedBody: { replace: ["aoe-review"] } },
      {
        label: "roots and replace",
        options: { roots: ["claude-user"], replace: ["aoe-review"] },
        expectedBody: { roots: ["claude-user"], replace: ["aoe-review"] },
      },
      {
        label: "directories only",
        options: { directories: ["aoe-review"] },
        expectedBody: { directories: ["aoe-review"] },
      },
    ];
    for (const { label, options, expectedBody } of cases) {
      fetchSpy.mockResolvedValueOnce(jsonResponse({ ok: true, outcomes: [] }));
      await syncSkills(options);
      const lastCall = fetchSpy.mock.calls[fetchSpy.mock.calls.length - 1];
      expect(lastCall[0], label).toBe("/api/skills/sync");
      expect(lastCall[1]?.method, label).toBe("POST");
      expect(JSON.parse(lastCall[1]?.body as string), label).toEqual(expectedBody);
    }

    fetchSpy.mockResolvedValueOnce(jsonResponse({ message: "read only" }, 403));
    expect(await syncSkills()).toEqual({
      ok: false,
      outcomes: [],
      error: "read only",
      status: 403,
    });
  });
});
