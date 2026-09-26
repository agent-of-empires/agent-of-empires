// @vitest-environment jsdom

import { describe, expect, it } from "vitest";
import { buildSidebar, resolveSelectedProfile } from "../SettingsView";

// Mirrors the TUI grouping; the pure config is asserted because the DOM renders the list twice.
describe("buildSidebar", () => {
  it("matches the TUI grouping order, with Profiles pinned first", () => {
    const order = buildSidebar().map((item) =>
      item.kind === "tab" ? { kind: item.kind, id: item.id, label: item.label } : item,
    );
    expect(order).toEqual([
      { kind: "tab", id: "profiles", label: "Profiles" },
      { kind: "divider", label: "Appearance" },
      { kind: "tab", id: "theme", label: "Theme" },
      { kind: "tab", id: "diff", label: "Diff" },
      { kind: "divider", label: "Sessions" },
      { kind: "tab", id: "session", label: "Session" },
      { kind: "tab", id: "structured-view", label: "Structured view" },
      { kind: "tab", id: "mcp", label: "MCP servers" },
      { kind: "tab", id: "skills", label: "Skills" },
      { kind: "divider", label: "Environment" },
      { kind: "tab", id: "sandbox", label: "Sandbox" },
      { kind: "tab", id: "worktree", label: "Worktree" },
      { kind: "tab", id: "tmux", label: "Tmux" },
      { kind: "divider", label: "Notifications" },
      { kind: "tab", id: "sound", label: "Sound" },
      { kind: "tab", id: "notifications", label: "Notifications" },
      { kind: "divider", label: "Web Dashboard" },
      { kind: "tab", id: "panels", label: "Panels" },
      { kind: "tab", id: "terminal", label: "Terminal" },
      { kind: "tab", id: "security", label: "Security" },
      { kind: "tab", id: "devices", label: "Devices" },
      { kind: "divider", label: "System" },
      { kind: "tab", id: "updates", label: "Updates" },
      { kind: "tab", id: "telemetry", label: "Telemetry" },
      { kind: "tab", id: "logging", label: "Logging" },
      { kind: "tab", id: "plugins", label: "Plugins" },
      { kind: "tab", id: "cityhall", label: "CityHall" },
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
