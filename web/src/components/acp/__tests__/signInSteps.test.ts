import { describe, expect, it } from "vitest";

import { signInSteps } from "../signInSteps";

describe("signInSteps", () => {
  it.each([
    ["claude", false, "claude auth login", "ANTHROPIC_API_KEY"],
    ["claude-code", true, "claude auth login", "ANTHROPIC_API_KEY"],
    ["codex", false, "codex login", "OPENAI_API_KEY"],
    // The browser sign-in's localhost callback can't reach a container.
    ["codex", true, "codex login --device-auth", "OPENAI_API_KEY"],
    ["opencode", false, null, null],
    [null, false, null, null],
  ])("%s (sandboxed: %s) signs in with %s", (agent, sandboxed, command, apiKeyEnv) => {
    expect(signInSteps(agent, "Agent", sandboxed)).toEqual({ agentName: "Agent", command, apiKeyEnv });
  });
});
