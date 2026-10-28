import { describe, expect, it } from "vitest";

import { queuedReason, type QueuedReasonInput } from "./queuedReason";

const BASE: QueuedReasonInput = {
  agent: "Codex",
  online: true,
  anyPending: false,
  waitingFor: null,
  rateLimitParked: false,
  rateLimit: null,
  workerDown: false,
  workerStarting: false,
  cancelling: false,
  compacting: false,
  turnActive: false,
  steerable: true,
  firstText: "hello",
  soloAliases: ["/new", "/compact"],
};

describe("queuedReason", () => {
  it("names the state that holds the queue, highest first", () => {
    const resetsAt = "2026-10-28T15:04:00.000Z";
    const at = new Date(resetsAt).toLocaleTimeString([], { hour: "2-digit", minute: "2-digit" });
    const cases: [string, Partial<QueuedReasonInput>, string, string | null, string][] = [
      ["idle", {}, "Queued", null, "send"],
      // Offline only matters once a row never reached the Daemon; it outranks a running turn.
      [
        "offline pending",
        { online: false, anyPending: true, turnActive: true },
        "Not sent",
        "sends when this dashboard reconnects",
        "dropOnly",
      ],
      ["offline, all confirmed", { online: false }, "Queued", null, "send"],
      [
        "new Session",
        { waitingFor: "Claude Code", workerDown: true },
        "Queued",
        "sends when Claude Code is ready",
        "send",
      ],
      [
        "rate limit with a reset",
        { rateLimitParked: true, rateLimit: { resets_at: resetsAt, status: "rejected" } },
        "Queued",
        `sends after the rate limit resets at ${at}`,
        "send",
      ],
      [
        "rate limit, unknown reset",
        { rateLimitParked: true, rateLimit: { resets_at: "nope", status: "rejected" } },
        "Queued",
        "sends after the rate limit lifts",
        "send",
      ],
      ["Worker starting", { workerStarting: true, workerDown: true }, "Queued", "sends when Codex starts", "disabled"],
      ["Worker down", { workerDown: true, turnActive: true }, "Queued", "sends when Codex starts", "startAndSend"],
      ["cancelling", { cancelling: true, turnActive: true }, "Queued", "sends once Codex stops", "disabled"],
      ["compacting", { compacting: true }, "Queued", "sends after compacting", "disabled"],
      [
        "solo command behind a turn",
        { turnActive: true, firstText: "  /compact keep tests" },
        "Queued",
        "/compact runs when this turn ends",
        "stopAndRun",
      ],
      [
        "a turn that can't be steered",
        { turnActive: true, steerable: false },
        "Queued",
        "sends when Codex finishes",
        "stopAndSend",
      ],
      // A steerable Agent takes the message mid-turn, so nothing holds it.
      ["a steerable turn", { turnActive: true }, "Queued", null, "send"],
    ];
    for (const [name, over, label, reason, action] of cases) {
      const got = queuedReason({ ...BASE, ...over });
      expect([got.label, got.reason, got.action], name).toEqual([label, reason, action]);
      expect(got.disabledReason !== undefined, name).toBe(action === "disabled");
      if (got.disabledReason) expect(got.disabledReason, name).toBe(`Waiting: this ${reason}`);
    }
  });
});
