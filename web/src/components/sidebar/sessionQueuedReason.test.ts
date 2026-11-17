import { describe, expect, it } from "vitest";

import { sessionQueuedReason, workspaceQueuedReason } from "./sessionQueuedReason";
import type { SessionResponse } from "../../lib/types";

const row = (fields: Partial<SessionResponse>) =>
  ({ id: "s", tool: "codex", acp_worker_state: "running", ...fields }) as SessionResponse;
const resets_at = "2026-01-01T15:30:00.000Z";
const at = new Date(resets_at).toLocaleTimeString([], { hour: "2-digit", minute: "2-digit" });
const limit = { status: "limited", resets_at, kind: "usage" };

describe("sessionQueuedReason", () => {
  it.each([
    ["a turn that can't take it", { queue_reason: "turn_active" }, "sends when Codex finishes"],
    ["a cancelling turn", { queue_reason: "cancelling" }, "sends once Codex stops"],
    ["compacting", { queue_reason: "compacting" }, "sends after compacting"],
    ["no worker", { queue_reason: "worker_down", acp_worker_state: "absent" }, "sends when Codex starts"],
    ["a worker starting", { queue_reason: "turn_active", acp_worker_state: "resuming" }, "sends when Codex starts"],
    ["a lost integration", { queue_reason: "integration_lost" }, null],
    [
      "a lost integration on a parked session",
      { queue_reason: "integration_lost", rate_limit: limit, rate_limit_auto_resume: true },
      null,
    ],
    ["nothing holding it", {}, null],
    [
      "a park that resumes by itself",
      { rate_limit: limit, rate_limit_auto_resume: true },
      `sends after the rate limit resets at ${at}`,
    ],
    [
      "a park with auto-resume off",
      { rate_limit: limit, rate_limit_auto_resume: false },
      "held until you resume: auto-resume is off",
    ],
    ["a park whose auto-resume isn't known", { rate_limit: limit }, "held by the rate limit"],
  ] as [string, Partial<SessionResponse>, string | null][])("words %s as the strip does", (_label, fields, want) => {
    expect(sessionQueuedReason(row(fields), "Codex")).toBe(want);
  });
});

describe("workspaceQueuedReason", () => {
  const name = (agent: string | null | undefined) => (agent === "codex" ? "Codex" : String(agent));
  const busy = row({ id: "a", queue_reason: "turn_active" });
  const compacting = row({ id: "b", queue_reason: "compacting" });
  it.each([
    [
      "one reason for every session with messages",
      [busy, row({ id: "b", queue_reason: "turn_active" })],
      [1, 2],
      "sends when Codex finishes",
    ],
    ["mixed reasons", [busy, compacting], [1, 1], null],
    ["a session without messages doesn't count", [busy, compacting], [1, 0], "sends when Codex finishes"],
    ["one session with no reason", [busy, row({ id: "b" })], [1, 1], null],
    ["no queued messages at all", [busy], [0], null],
  ] as [string, SessionResponse[], number[], string | null][])("%s", (_label, sessions, counts, want) => {
    expect(workspaceQueuedReason(sessions, counts, name)).toBe(want);
  });
});
