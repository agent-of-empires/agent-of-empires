import { expect, it } from "vitest";
import { DEFAULT_AGENT_PROFILE, isClearAlias, usableAgentProfile, type AgentProfile } from "./agentProfile";

it.each<[string, string[], boolean]>([
  ["/clear", ["/clear"], true],
  ["/new", ["/new"], true],
  ["  /clear  ", ["/clear"], true],
  ["\n/clear\n", ["/clear"], true],
  ["/clear --hard", ["/clear"], true],
  ["clear", ["/clear"], false],
  ["/cleart", ["/clear"], false],
  ["hello /clear world", ["/clear"], false],
  ["", ["/clear"], false],
  ["/clear", [], false],
  ["/new", ["/clear"], false],
])("isClearAlias(%j, %j) is %s", (text, aliases, expected) => {
  expect(isClearAlias(text, aliases)).toBe(expected);
});

it("usableAgentProfile ignores a profile with an unknown version", () => {
  const monitor = { ...DEFAULT_AGENT_PROFILE, mobile_monitor_only: true };
  expect(usableAgentProfile(monitor).mobile_monitor_only).toBe(true);
  expect(usableAgentProfile({ ...monitor, v: 2 } as unknown as AgentProfile)).toBe(DEFAULT_AGENT_PROFILE);
  expect(usableAgentProfile(undefined)).toBe(DEFAULT_AGENT_PROFILE);
});
