// @vitest-environment jsdom
//
// Unit coverage for SettingsView's `resolveSelectedProfile` helper. This is
// the post-mount-fetch decision that closes the race where a user-set
// selection would otherwise be silently reverted by the unconditional
// `setSelectedProfile(active.name)` the helper replaced.
//
// The full end-to-end behavior is asserted by the mocked Playwright spec
// `web/tests/profile-lifecycle.spec.ts`. This test focuses on the
// branch logic.

import { describe, expect, it } from "vitest";
import { buildSidebar, resolveSelectedProfile } from "../SettingsView";

// Story #1: the web sidebar divider/tab order mirrors the TUI grouping
// (categories_for_scope() in src/tui/settings/mod.rs) so muscle memory carries
// across surfaces. Asserting the pure config is more robust than querying the
// DOM, which renders the same list twice (mobile strip + desktop nav).
describe("buildSidebar", () => {
  it("matches the TUI grouping order, with Profiles pinned first", () => {
    // Strip the Profiles tab's icon (a ReactNode) so the order assertion stays
    // a plain structural compare; the icon presence is asserted separately.
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
      { kind: "divider", label: "Environment" },
      { kind: "tab", id: "sandbox", label: "Sandbox" },
      { kind: "tab", id: "worktree", label: "Worktree" },
      { kind: "tab", id: "tmux", label: "Tmux" },
      { kind: "divider", label: "Notifications" },
      { kind: "tab", id: "sound", label: "Sound" },
      { kind: "tab", id: "notifications", label: "Notifications" },
      { kind: "divider", label: "Web Dashboard" },
      { kind: "tab", id: "terminal", label: "Terminal" },
      { kind: "tab", id: "security", label: "Security" },
      { kind: "tab", id: "devices", label: "Devices" },
      { kind: "divider", label: "System" },
      { kind: "tab", id: "updates", label: "Updates" },
      { kind: "tab", id: "telemetry", label: "Telemetry" },
      { kind: "tab", id: "logging", label: "Logging" },
    ]);
  });

  it("gives the Profiles tab an icon", () => {
    const profiles = buildSidebar().find((item) => item.kind === "tab" && item.id === "profiles");
    expect(profiles && "icon" in profiles && profiles.icon).toBeTruthy();
  });
});

describe("resolveSelectedProfile", () => {
  it("preserves the current selection when it still exists in the profile list", () => {
    const profiles = [
      { name: "default", is_default: true },
      { name: "work", is_default: false },
    ];
    expect(resolveSelectedProfile("work", profiles)).toBe("work");
  });

  it("preserves the current selection even when it is the default-flagged profile", () => {
    const profiles = [
      { name: "default", is_default: true },
      { name: "work", is_default: false },
    ];
    expect(resolveSelectedProfile("default", profiles)).toBe("default");
  });

  it("falls back to the default-flagged profile when the current selection was deleted", () => {
    const profiles = [
      { name: "default", is_default: false },
      { name: "work", is_default: true },
    ];
    expect(resolveSelectedProfile("scratch", profiles)).toBe("work");
  });

  it("falls back to the literal 'default' string when neither current nor default-flagged exists", () => {
    const profiles = [{ name: "scratch", is_default: false }];
    expect(resolveSelectedProfile("missing", profiles)).toBe("default");
  });

  it("falls back to 'default' on an empty profile list (boundary)", () => {
    expect(resolveSelectedProfile("anything", [])).toBe("default");
  });
});
