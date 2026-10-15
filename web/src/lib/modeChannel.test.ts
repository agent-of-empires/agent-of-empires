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
      allowLegacyFallback: true,
    });
    expect(channel!.modes.some((m) => m.id === "default")).toBe(false);
  });

  it("reflects an in-flight switch only for the mode control", () => {
    const pending = (configId: string, value: string) =>
      resolveModeChannel({ ...BASE, configOptions: [OPENCODE_MODE_OPTION], pendingConfigOption: { configId, value } });
    expect(pending("mode", "plan")).toMatchObject({ pendingId: "plan", activeId: "build" });
    expect(pending("model", "claude-opus-4-8")!.pendingId).toBeNull();
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
    expect(channel!.modes.map((m) => m.id)).toEqual(["default", "plan", "accept_edits", "bypass_permissions"]);
    expect(channel!.activeId).toBe("default");
    expect(resolveModeChannel({ ...BASE, legacyMode: "Plan", allowLegacyFallback: true })!.activeId).toBe("plan");
  });

  it("renders nothing without advertised modes or with an empty mode option", () => {
    expect(resolveModeChannel(BASE)).toBeNull();
    expect(resolveModeChannel({ ...BASE, configOptions: [{ ...OPENCODE_MODE_OPTION, options: [] }] })).toBeNull();
  });
});
