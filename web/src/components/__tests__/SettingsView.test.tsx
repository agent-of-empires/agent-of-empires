// @vitest-environment jsdom

import { describe, expect, it } from "vitest";
import { buildSidebar, resolveSelectedProfile } from "../SettingsView";

// The pure config is asserted because the DOM renders the list twice.
describe("buildSidebar", () => {
  it("leads with the dashboard group, then sessions, environment and system", () => {
    const order = buildSidebar().map((item) => (item.kind === "tab" ? item.id : `-- ${item.label}`));
    expect(order).toEqual([
      "-- Dashboard",
      "theme",
      "notifications",
      "terminal",
      "panels",
      "diff",
      "devices",
      "security",
      "-- Sessions",
      "profiles",
      "session",
      "structured-view",
      "mcp",
      "skills",
      "-- Environment",
      "sandbox",
      "worktree",
      "tmux",
      "sound",
      "-- System",
      "updates",
      "telemetry",
      "logging",
      "plugins",
      "cityhall",
    ]);
  });
});

describe("resolveSelectedProfile", () => {
  const both = (defaultName: string) => [
    { name: "default", is_default: defaultName === "default" },
    { name: "work", is_default: defaultName === "work" },
  ];
  it.each([
    ["keeps a still-valid selection", "work", both("default"), "work"],
    ["falls back to the default-flagged profile", "scratch", both("work"), "work"],
    ["falls back to 'default' with no default flag", "missing", [{ name: "scratch", is_default: false }], "default"],
  ])("%s", (_n, current, profiles, expected) => {
    expect(resolveSelectedProfile(current, profiles)).toBe(expected);
  });
});
