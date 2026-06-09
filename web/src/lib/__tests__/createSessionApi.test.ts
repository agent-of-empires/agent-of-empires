// Vitest coverage for the createSession API client's hooks-trust handling
// (#2066): a `hooks_need_trust` 403 is surfaced as a structured
// `hooksNeedTrust` field (commands + MCP flag) rather than a bare error
// string, so the wizard can prompt and resubmit with `trust_hooks: true`.

import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import { createSession } from "../api";
import type { CreateSessionRequest } from "../types";

const fetchSpy = vi.fn<typeof fetch>();

const BODY: CreateSessionRequest = { path: "/repo", tool: "claude" };

beforeEach(() => {
  fetchSpy.mockReset();
  vi.stubGlobal("fetch", fetchSpy);
});

afterEach(() => {
  vi.unstubAllGlobals();
});

describe("createSession hooks-trust handling (#2066)", () => {
  it("surfaces hooksNeedTrust from a hooks_need_trust 403", async () => {
    fetchSpy.mockResolvedValue(
      new Response(
        JSON.stringify({
          error: "hooks_need_trust",
          message: "Repository hooks require trust.",
          on_create: ["bash scripts/setup-worktree.sh"],
          on_launch: ["npm start"],
          on_destroy: ["rm /tmp/seed"],
          needs_mcp_trust: true,
        }),
        { status: 403 },
      ),
    );

    const result = await createSession(BODY);

    expect(result.ok).toBe(false);
    expect(result.hooksNeedTrust).toEqual({
      onCreate: ["bash scripts/setup-worktree.sh"],
      onLaunch: ["npm start"],
      onDestroy: ["rm /tmp/seed"],
      needsMcpTrust: true,
    });
    expect(result.error).toBe("Repository hooks require trust.");
  });

  it("defaults the hook lists to [] and needsMcpTrust to false when omitted", async () => {
    fetchSpy.mockResolvedValue(
      new Response(JSON.stringify({ error: "hooks_need_trust", message: "trust me" }), { status: 403 }),
    );

    const result = await createSession(BODY);

    expect(result.hooksNeedTrust).toEqual({ onCreate: [], onLaunch: [], onDestroy: [], needsMcpTrust: false });
  });

  it("forwards trust_hooks: true in the request body", async () => {
    fetchSpy.mockResolvedValue(new Response(JSON.stringify({ id: "new-session" }), { status: 201 }));

    const result = await createSession({ ...BODY, trust_hooks: true });

    expect(result.ok).toBe(true);
    const init = fetchSpy.mock.calls[0][1];
    expect(JSON.parse(String(init?.body))).toMatchObject({ trust_hooks: true });
  });

  it("treats a non-hooks error 4xx as a plain error without hooksNeedTrust", async () => {
    fetchSpy.mockResolvedValue(
      new Response(JSON.stringify({ error: "create_failed", message: "branch exists" }), { status: 400 }),
    );

    const result = await createSession(BODY);

    expect(result.ok).toBe(false);
    expect(result.hooksNeedTrust).toBeUndefined();
    expect(result.error).toBe("branch exists");
  });
});
