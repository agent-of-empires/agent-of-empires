import { describe, expect, it } from "vitest";
import { isAcpCapable, isAcpEligible } from "../acpCapableTools";

describe("isAcpEligible", () => {
  it("denies only on an explicit acp_allowed: false", () => {
    const cases: [string, { acp_capable?: boolean; acp_allowed?: boolean } | undefined, boolean][] = [
      ["claude", { acp_capable: true, acp_allowed: true }, true],
      ["claude", { acp_capable: true, acp_allowed: false }, false],
      ["claude", { acp_capable: false, acp_allowed: true }, false],
      ["claude", { acp_capable: false, acp_allowed: false }, false],
      ["claude", { acp_capable: true }, true],
      ["claude", { acp_capable: false }, false],
      ["claude", undefined, true],
      ["kimi", undefined, true],
      ["prime-agent", undefined, true],
      ["some-unknown-tool", undefined, false],
      ["claude", { acp_allowed: false }, false],
      ["claude", { acp_allowed: true }, true],
    ];
    for (const [tool, agent, expected] of cases) {
      expect(isAcpEligible(tool, agent), `${tool} ${JSON.stringify(agent)}`).toBe(expected);
    }
  });

  it("leaves isAcpCapable reporting capability alone, so settings surfaces keep listing a denied agent", () => {
    expect(isAcpCapable("codex", true)).toBe(true);
    expect(isAcpEligible("codex", { acp_capable: true, acp_allowed: false })).toBe(false);
  });
});
