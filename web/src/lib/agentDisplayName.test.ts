import { describe, expect, it } from "vitest";

import { agentDisplayName } from "./agentDisplayName";

describe("agentDisplayName", () => {
  it.each([
    ["claude", "Claude Code"],
    ["my-agent", "my-agent"],
    // Custom registry keys that are also Object.prototype members keep their own name.
    ["constructor", "constructor"],
    ["toString", "toString"],
    ["__proto__", "__proto__"],
    [null, "the Agent"],
  ])("%s → %s", (agent, name) => {
    expect(agentDisplayName(agent)).toBe(name);
  });
});
