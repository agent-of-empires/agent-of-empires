import { describe, expect, it } from "vitest";
import { resolveLaunchCommand } from "./launchCommand";

describe("resolveLaunchCommand (structured view)", () => {
  const base = { tool: "opencode", useStructuredView: true, binary: "opencode" };

  it("appends the registry args to the structured view command (the #1911 case)", () => {
    expect(
      resolveLaunchCommand({
        ...base,
        acpCommand: "opencode",
        acpArgs: ["acp"],
      }),
    ).toEqual({ prefix: "opencode", suffix: "acp", full: "opencode acp" });
  });

  it("prefers acp_command over binary (claude -> claude-agent-acp)", () => {
    expect(
      resolveLaunchCommand({
        tool: "claude",
        useStructuredView: true,
        binary: "claude",
        acpCommand: "claude-agent-acp",
        acpArgs: [],
      }),
    ).toEqual({
      prefix: "claude-agent-acp",
      suffix: "",
      full: "claude-agent-acp",
    });
  });

  it("retains the registry args when a config override applies, without duplicating them", () => {
    const r = resolveLaunchCommand({
      ...base,
      acpCommand: "opencode",
      acpArgs: ["acp"],
      agentCommandOverride: { opencode: "opencode-plannotator" },
    });
    expect(r.full).toBe("opencode-plannotator acp");
    expect(r.prefix).toBe("opencode-plannotator");
    expect(r.suffix).toBe("acp");
  });

  it("manual override wins over the config override, args still retained", () => {
    expect(
      resolveLaunchCommand({
        ...base,
        acpCommand: "opencode",
        acpArgs: ["acp"],
        manualOverride: "opencode --foo",
        agentCommandOverride: { opencode: "opencode-plannotator" },
      }),
    ).toEqual({
      prefix: "opencode --foo",
      suffix: "acp",
      full: "opencode --foo acp",
    });
  });

  it("editing the prefix back into the override does not double-append registry args", () => {
    // Simulate the wizard writing the edited prefix into commandOverride.
    const edited = "opencode-plannotator";
    const r = resolveLaunchCommand({
      ...base,
      acpCommand: "opencode",
      acpArgs: ["acp"],
      manualOverride: edited,
    });
    expect(r.full).toBe("opencode-plannotator acp");
    expect(r.full.match(/acp/g)?.length).toBe(1);
  });

  it("strips a duplicated registry-arg suffix from an override (no double-append)", () => {
    const r = resolveLaunchCommand({
      ...base,
      acpCommand: "opencode",
      acpArgs: ["acp"],
      manualOverride: "opencode acp",
    });
    expect(r).toEqual({ prefix: "opencode", suffix: "acp", full: "opencode acp" });
  });

  it("falls back to the tool name when no command is known", () => {
    expect(resolveLaunchCommand({ tool: "opencode", useStructuredView: true })).toEqual(
      { prefix: "opencode", suffix: "", full: "opencode" },
    );
  });

  it("ignores a whitespace-only manual override", () => {
    expect(
      resolveLaunchCommand({
        ...base,
        acpCommand: "opencode",
        acpArgs: ["acp"],
        manualOverride: "   ",
        agentCommandOverride: { opencode: "opencode-plannotator" },
      }).full,
    ).toBe("opencode-plannotator acp");
  });
});

describe("resolveLaunchCommand (tmux)", () => {
  it("uses the binary plus the session extra args", () => {
    expect(
      resolveLaunchCommand({
        tool: "claude",
        useStructuredView: false,
        binary: "claude",
        extraArgs: "--model opus",
      }),
    ).toEqual({ prefix: "claude", suffix: "--model opus", full: "claude --model opus" });
  });

  it("ignores acp_args when not in structured view", () => {
    expect(
      resolveLaunchCommand({
        tool: "opencode",
        useStructuredView: false,
        binary: "opencode",
        acpArgs: ["acp"],
      }),
    ).toEqual({ prefix: "opencode", suffix: "", full: "opencode" });
  });

  it("falls back to custom_agents when no manual or config override", () => {
    expect(
      resolveLaunchCommand({
        tool: "my-agent",
        useStructuredView: false,
        binary: "my-agent",
        customAgents: { "my-agent": "my-agent-wrapper run" },
      }).prefix,
    ).toBe("my-agent-wrapper run");
  });
});
