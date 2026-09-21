import { expect, it } from "vitest";
import { resolveLaunchCommand, type ResolveLaunchCommandInput } from "./launchCommand";

const opencode = {
  tool: "opencode",
  useStructuredView: true,
  binary: "opencode",
  acpCommand: "opencode",
  acpArgs: ["acp"],
};
const planner = { opencode: "opencode-plannotator" };

it.each<[string, ResolveLaunchCommandInput, string, string]>([
  ["appends registry args (#1911)", opencode, "opencode", "acp"],
  [
    "prefers acp_command over binary",
    { tool: "claude", useStructuredView: true, binary: "claude", acpCommand: "claude-agent-acp", acpArgs: [] },
    "claude-agent-acp",
    "",
  ],
  [
    "keeps registry args with a config override",
    { ...opencode, agentCommandOverride: planner },
    "opencode-plannotator",
    "acp",
  ],
  [
    "lets a manual override beat the config override",
    { ...opencode, manualOverride: "opencode --foo", agentCommandOverride: planner },
    "opencode --foo",
    "acp",
  ],
  [
    "does not double-append after editing the prefix",
    { ...opencode, manualOverride: "opencode-plannotator" },
    "opencode-plannotator",
    "acp",
  ],
  ["strips a duplicated suffix from an override", { ...opencode, manualOverride: "opencode acp" }, "opencode", "acp"],
  ["falls back to the tool name", { tool: "opencode", useStructuredView: true }, "opencode", ""],
  [
    "ignores a blank manual override",
    { ...opencode, manualOverride: "   ", agentCommandOverride: planner },
    "opencode-plannotator",
    "acp",
  ],
  [
    "uses tmux extra args",
    { tool: "claude", useStructuredView: false, binary: "claude", extraArgs: "--model opus" },
    "claude",
    "--model opus",
  ],
  ["ignores acp_args for tmux", { ...opencode, useStructuredView: false, acpCommand: undefined }, "opencode", ""],
  [
    "falls back to custom_agents",
    {
      tool: "my-agent",
      useStructuredView: false,
      binary: "my-agent",
      customAgents: { "my-agent": "my-agent-wrapper run" },
    },
    "my-agent-wrapper run",
    "",
  ],
])("%s", (_name, input, prefix, suffix) => {
  expect(resolveLaunchCommand(input)).toEqual({ prefix, suffix, full: suffix ? `${prefix} ${suffix}` : prefix });
});
