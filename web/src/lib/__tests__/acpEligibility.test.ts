import { describe, expect, it } from "vitest";
import { acpAdapterMissing, isAcpCapable, isAcpEligible, structuredAvailable } from "../acpCapableTools";

describe("isAcpEligible", () => {
  it("denies only on an explicit acp_allowed: false", () => {
    const cases: [
      string,
      { acp_capable?: boolean; acp_allowed?: boolean; acp_create_ready?: boolean } | undefined,
      boolean,
    ][] = [
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
      // A repository alias can make a Terminal-only agent create-ready.
      ["aider", { acp_capable: false, acp_create_ready: true }, true],
      ["aider", { acp_capable: false, acp_create_ready: true, acp_allowed: false }, false],
      ["aider", { acp_capable: false, acp_create_ready: false }, false],
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

describe("acpAdapterMissing", () => {
  const host = { sandboxEnabled: false, commandOverride: "" };
  it.each([
    ["not ready on a host launch", { acp_create_ready: false }, host, true],
    ["ready", { acp_create_ready: true }, host, false],
    ["an older server without the flag", {}, host, false],
    ["no agent entry yet", undefined, host, false],
    ["installed but not create-ready", { acp_installed: true, acp_create_ready: false }, host, true],
    ["not installed but create-ready", { acp_installed: false, acp_create_ready: true }, host, false],
    ["Sandbox on", { acp_create_ready: false }, { ...host, sandboxEnabled: true }, false],
    ["a command override", { acp_create_ready: false }, { ...host, commandOverride: "my-wrapper acp" }, false],
    ["a blank command override", { acp_create_ready: false }, { ...host, commandOverride: "   " }, true],
  ])("%s", (_label, agent, launch, expected) => {
    expect(acpAdapterMissing(agent, launch)).toBe(expected);
  });

  it("leaves Structured view available unless the adapter is missing", () => {
    expect(structuredAvailable("claude", { acp_create_ready: false }, host)).toBe(false);
    expect(structuredAvailable("claude", { acp_create_ready: true }, host)).toBe(true);
    expect(structuredAvailable("aider", { acp_capable: false, acp_create_ready: true }, host)).toBe(true);
    expect(structuredAvailable("aider", { acp_capable: false }, host)).toBe(false);
    expect(structuredAvailable("claude", { acp_allowed: false, acp_create_ready: true }, host)).toBe(false);
    expect(structuredAvailable("claude", { acp_create_ready: false }, { ...host, sandboxEnabled: true })).toBe(true);
    expect(
      structuredAvailable("claude", { acp_create_ready: false }, { ...host, commandOverride: "my-wrapper acp" }),
    ).toBe(true);
  });
});
