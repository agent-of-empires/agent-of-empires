// @vitest-environment jsdom
import { renderHook } from "@testing-library/react";
import { expect, it } from "vitest";

import { DEFAULT_AGENT_PROFILE, type AgentProfile } from "./agentProfile";
import { useStableAgentProfile } from "./agentProfileContext";
import { CLAUDE_PROFILE, CODEX_PROFILE } from "./__tests__/agentProfileFixtures";

function run(initial: AgentProfile | null | undefined) {
  return renderHook(({ p }) => useStableAgentProfile(p), { initialProps: { p: initial } });
}

it("keeps identity for equal content and follows a real change", () => {
  const { result, rerender } = run(CLAUDE_PROFILE);
  const first = result.current;
  rerender({ p: structuredClone(CLAUDE_PROFILE) });
  expect(result.current).toBe(first);
  rerender({ p: CODEX_PROFILE });
  expect(result.current).toBe(CODEX_PROFILE);
});

it.each<[string, AgentProfile | null | undefined]>([
  ["an unknown version", { ...CLAUDE_PROFILE, v: 2 }],
  ["null", null],
  ["undefined", undefined],
])("falls back to the default for %s", (_label, profile) => {
  expect(run(profile).result.current).toBe(DEFAULT_AGENT_PROFILE);
});
