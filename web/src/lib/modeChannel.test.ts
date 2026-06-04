import { describe, expect, it } from "vitest";
import type { ConfigOptionDescriptor } from "./acpTypes";
import { resolveModeChannel, type ResolveModeChannelArgs } from "./modeChannel";

const BASE: ResolveModeChannelArgs = {
  configOptions: [],
  availableModes: [],
  currentModeId: null,
  legacyMode: "Default",
  pendingConfigOption: null,
  allowLegacyFallback: false,
};

const OPENCODE_MODE_OPTION: ConfigOptionDescriptor = {
  id: "mode",
  name: "Session Mode",
  category: "mode",
  current_value: "build",
  options: [
    { value: "build", name: "Build" },
    { value: "plan", name: "Plan" },
  ],
};

describe("resolveModeChannel", () => {
  it("prefers the config-option channel and switches via set_config_option", () => {
    const channel = resolveModeChannel({
      ...BASE,
      configOptions: [OPENCODE_MODE_OPTION],
    });
    expect(channel).not.toBeNull();
    expect(channel!.kind).toBe("config");
    expect(channel!.configId).toBe("mode");
    expect(channel!.activeId).toBe("build");
    expect(channel!.modes.map((m) => m.id)).toEqual(["build", "plan"]);
    expect(channel!.label).toBe("Session Mode");
  });

  it("never offers a phantom 'default' mode for config-backed agents (#1764)", () => {
    const channel = resolveModeChannel({
      ...BASE,
      configOptions: [OPENCODE_MODE_OPTION],
      // Even with the claude fallback allowed, a real config option wins
      // so OpenCode's user is never shown a mode it would reject.
      allowLegacyFallback: true,
    });
    expect(channel!.modes.some((m) => m.id === "default")).toBe(false);
  });

  it("reflects an in-flight config switch as the pending id", () => {
    const channel = resolveModeChannel({
      ...BASE,
      configOptions: [OPENCODE_MODE_OPTION],
      pendingConfigOption: { configId: "mode", value: "plan" },
    });
    expect(channel!.pendingId).toBe("plan");
    // Active stays put (pessimistic UI) until the adapter confirms.
    expect(channel!.activeId).toBe("build");
  });

  it("ignores a pending config option for a different control", () => {
    const channel = resolveModeChannel({
      ...BASE,
      configOptions: [OPENCODE_MODE_OPTION],
      pendingConfigOption: { configId: "model", value: "claude-opus-4-8" },
    });
    expect(channel!.pendingId).toBeNull();
  });

  it("falls back to the SessionModeState channel and switches via set_mode", () => {
    const channel = resolveModeChannel({
      ...BASE,
      availableModes: [
        { id: "default", name: "Default", description: null },
        { id: "plan", name: "Plan", description: null },
      ],
      currentModeId: "plan",
    });
    expect(channel!.kind).toBe("legacy");
    expect(channel!.configId).toBeNull();
    expect(channel!.activeId).toBe("plan");
    expect(channel!.pendingId).toBeNull();
  });

  it("uses the claude hardcoded taxonomy only when the profile opts in", () => {
    const channel = resolveModeChannel({ ...BASE, allowLegacyFallback: true });
    expect(channel!.kind).toBe("legacy");
    expect(channel!.modes.map((m) => m.id)).toEqual([
      "default",
      "plan",
      "accept_edits",
      "bypass_permissions",
    ]);
    expect(channel!.activeId).toBe("default");
  });

  it("maps the legacy SessionMode enum to the fallback active id", () => {
    const channel = resolveModeChannel({
      ...BASE,
      legacyMode: "Plan",
      allowLegacyFallback: true,
    });
    expect(channel!.activeId).toBe("plan");
  });

  it("renders nothing for a non-claude agent that advertised no modes", () => {
    expect(resolveModeChannel(BASE)).toBeNull();
  });

  it("ignores an empty mode config option", () => {
    const empty: ConfigOptionDescriptor = {
      ...OPENCODE_MODE_OPTION,
      options: [],
    };
    expect(resolveModeChannel({ ...BASE, configOptions: [empty] })).toBeNull();
  });
});
