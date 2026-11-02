import { describe, expect, it } from "vitest";

import { queuedReason, type QueuedReasonInput } from "./queuedReason";

const BASE: QueuedReasonInput = {
  agent: "Codex",
  online: true,
  anyPending: false,
  waitingFor: null,
  rateLimitParked: false,
  rateLimit: null,
  rateLimitAutoResume: true,
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
  it("names the state that holds the queue, highest first, with each row kind's action", () => {
    const resetsAt = "2026-10-28T15:04:00.000Z";
    const at = new Date(resetsAt).toLocaleTimeString([], { hour: "2-digit", minute: "2-digit" });
    const parked = { rateLimitParked: true, rateLimit: { resets_at: resetsAt, status: "rejected" } };
    // (name, input, label, reason, message row action, solo command row action)
    const cases: [string, Partial<QueuedReasonInput>, string, string | null, string, string][] = [
      ["idle", {}, "Queued", null, "send", "send"],
      // Offline only matters once a row never reached the Daemon; it outranks a running turn.
      [
        "offline pending",
        { online: false, anyPending: true, turnActive: true },
        "Not sent",
        "sends when this dashboard reconnects",
        "dropOnly",
        "dropOnly",
      ],
      ["offline, all confirmed", { online: false }, "Queued", null, "send", "send"],
      // Online, an unconfirmed row is still on its way to the Daemon.
      ["online, a row unconfirmed", { anyPending: true }, "Queuing", null, "send", "send"],
      [
        "new Session",
        { waitingFor: "Claude Code", workerDown: true },
        "Queued",
        "sends when Claude Code is ready",
        "send",
        "send",
      ],
      // Only an armed auto-resume promises delivery; Send now stays in every case.
      ["rate limit, auto-resume armed", parked, "Queued", `sends after the rate limit resets at ${at}`, "send", "send"],
      [
        "rate limit, armed, unknown reset",
        { ...parked, rateLimit: { resets_at: "nope", status: "rejected" } },
        "Queued",
        "sends after the rate limit lifts",
        "send",
        "send",
      ],
      [
        "rate limit, auto-resume off",
        { ...parked, rateLimitAutoResume: false },
        "Queued",
        "held until you resume: auto-resume is off",
        "send",
        "send",
      ],
      [
        "rate limit, auto-resume not reported",
        { ...parked, rateLimitAutoResume: undefined },
        "Queued",
        "held by the rate limit",
        "send",
        "send",
      ],
      [
        "Worker starting",
        { workerStarting: true, workerDown: true },
        "Queued",
        "sends when Codex starts",
        "disabled",
        "disabled",
      ],
      [
        "Worker down",
        { workerDown: true, turnActive: true },
        "Queued",
        "sends when Codex starts",
        "startAndSend",
        "startAndSend",
      ],
      [
        "cancelling",
        { cancelling: true, turnActive: true },
        "Queued",
        "sends once Codex stops",
        "disabled",
        "disabled",
      ],
      ["compacting", { compacting: true }, "Queued", "sends after compacting", "disabled", "disabled"],
      // A steerable turn takes a message now, but a solo command still needs the turn stopped.
      [
        "solo command behind a steerable turn",
        { turnActive: true, firstText: "  /compact keep tests" },
        "Queued",
        "/compact runs when this turn ends",
        "send",
        "stopAndRun",
      ],
      [
        "solo command behind a turn that can't be steered",
        { turnActive: true, steerable: false, firstText: "/new" },
        "Queued",
        "/new runs when this turn ends",
        "stopAndSend",
        "stopAndRun",
      ],
      [
        "a turn that can't be steered",
        { turnActive: true, steerable: false },
        "Queued",
        "sends when Codex finishes",
        "stopAndSend",
        "stopAndRun",
      ],
      ["a steerable turn", { turnActive: true }, "Queued", null, "send", "stopAndRun"],
    ];
    for (const [name, over, label, reason, action, soloAction] of cases) {
      const got = queuedReason({ ...BASE, ...over });
      expect([got.label, got.reason, got.action, got.soloAction], name).toEqual([label, reason, action, soloAction]);
      expect(got.disabledReason !== undefined, name).toBe(action === "disabled");
      if (got.disabledReason) expect(got.disabledReason, name).toBe(`Waiting: this message ${reason}`);
    }
  });
});
