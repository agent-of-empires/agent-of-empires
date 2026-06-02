import { describe, expect, it } from "vitest";
import { resolveLaunchCommand } from "./launchCommand";

describe("resolveLaunchCommand", () => {
  it("uses session.agent_command_override when no manual override (the #1766 case)", () => {
    expect(
      resolveLaunchCommand({
        tool: "opencode",
        binary: "opencode",
        agentCommandOverride: { opencode: "opencode-plannotator" },
      }),
    ).toBe("opencode-plannotator");
  });

  it("manual override wins over the config override", () => {
    expect(
      resolveLaunchCommand({
        tool: "opencode",
        binary: "opencode",
        manualOverride: "opencode --foo",
        agentCommandOverride: { opencode: "opencode-plannotator" },
      }),
    ).toBe("opencode --foo");
  });

  it("falls back to custom_agents when no manual or agent override", () => {
    expect(
      resolveLaunchCommand({
        tool: "my-agent",
        binary: "my-agent",
        customAgents: { "my-agent": "my-agent-wrapper run" },
      }),
    ).toBe("my-agent-wrapper run");
  });

  it("falls back to the agent binary when nothing is overridden", () => {
    expect(resolveLaunchCommand({ tool: "claude", binary: "claude-agent-acp" })).toBe(
      "claude-agent-acp",
    );
  });

  it("falls back to the tool name when no binary is known", () => {
    expect(resolveLaunchCommand({ tool: "opencode" })).toBe("opencode");
  });

  it("ignores a whitespace-only manual override", () => {
    expect(
      resolveLaunchCommand({
        tool: "opencode",
        binary: "opencode",
        manualOverride: "   ",
        agentCommandOverride: { opencode: "opencode-plannotator" },
      }),
    ).toBe("opencode-plannotator");
  });

  it("ignores an empty-string config override entry", () => {
    expect(
      resolveLaunchCommand({
        tool: "opencode",
        binary: "opencode",
        agentCommandOverride: { opencode: "" },
      }),
    ).toBe("opencode");
  });
});
