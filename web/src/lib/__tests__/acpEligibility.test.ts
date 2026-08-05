import { describe, expect, it } from "vitest";
import { isAcpCapable, isAcpEligible } from "../acpCapableTools";

/** #3241: the wizard must not offer a structured session for an agent the
 *  operator's `[acp] allowed_agents` policy refuses, because the create call
 *  would then come back 403. Capability and permission stay separate axes, so
 *  `isAcpEligible` is the conjunction and `isAcpCapable` keeps its old meaning. */
describe("isAcpEligible", () => {
  it("denies only on an explicit acp_allowed: false", () => {
    const cases: [string, { acp_capable?: boolean; acp_allowed?: boolean } | undefined, boolean][] = [
      // Permitted and capable: eligible.
      ["claude", { acp_capable: true, acp_allowed: true }, true],
      // Refused by policy, even though the adapter exists.
      ["claude", { acp_capable: true, acp_allowed: false }, false],
      // Not capable at all: still ineligible, policy is irrelevant.
      ["claude", { acp_capable: false, acp_allowed: true }, false],
      ["claude", { acp_capable: false, acp_allowed: false }, false],
      // Field absent (older server, or a fixture predating it): treated as
      // permitted so the wizard does not lock itself out against a server that
      // never reports it. Falls through to the capability flag.
      ["claude", { acp_capable: true }, true],
      ["claude", { acp_capable: false }, false],
      // No agent record yet (the list is still loading): falls back to the
      // hardcoded tool set, same as isAcpCapable.
      ["claude", undefined, true],
      ["some-unknown-tool", undefined, false],
      // A policy denial wins even before the capability flag has arrived.
      ["claude", { acp_allowed: false }, false],
      ["claude", { acp_allowed: true }, true],
    ];
    for (const [tool, agent, expected] of cases) {
      expect(isAcpEligible(tool, agent), `${tool} ${JSON.stringify(agent)}`).toBe(expected);
    }
  });

  it("leaves isAcpCapable reporting capability alone, so settings surfaces keep listing a denied agent", () => {
    // The reason `acp_capable` was not overloaded: an operator editing
    // per-agent structured-view defaults for an agent that is currently off the
    // allowlist is a legitimate thing to do.
    expect(isAcpCapable("codex", true)).toBe(true);
    expect(isAcpEligible("codex", { acp_capable: true, acp_allowed: false })).toBe(false);
  });
});
