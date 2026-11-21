import { describe, expect, it } from "vitest";

import { agentDefaultsHref } from "./agentModelDefault";

describe("agentDefaultsHref", () => {
  it.each([
    ["before /api/about answers", null, null],
    ["read-only", { read_only: true, cityhall_mode: false }, null],
    ["CityHall", { read_only: false, cityhall_mode: true }, null],
    [
      "writable",
      { read_only: false, cityhall_mode: false },
      "/settings/structured-view?profile=my%20work&field=acp.acp_defaults",
    ],
  ])("%s", (_label, about, want) => {
    expect(agentDefaultsHref("my work", about as Parameters<typeof agentDefaultsHref>[1])).toBe(want);
  });
});
