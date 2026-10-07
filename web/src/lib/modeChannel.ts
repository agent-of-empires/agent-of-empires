// Mode-picker channel resolution: read and write through the same channel, in precedence order: a "mode" config option (set_config_option), then ACP SessionModeState (set_mode), then Claude's legacy taxonomy when the profile opts in. Otherwise no picker.

import type { AcpState, ConfigOptionDescriptor, SessionMode } from "./acpTypes";

/** Only used when the profile sets `capabilities.legacyModeFallback`. */
export const LEGACY_MODES: ReadonlyArray<{
  id: string;
  legacyId: SessionMode;
  name: string;
  description: string;
}> = [
  {
    id: "default",
    legacyId: "Default",
    name: "Default",
    description: "Approve each tool individually",
  },
  {
    id: "plan",
    legacyId: "Plan",
    name: "Plan",
    description: "Plan first, no edits applied",
  },
  {
    id: "accept_edits",
    legacyId: "AcceptEdits",
    name: "Accept edits",
    description: "Auto-approve safe file edits",
  },
  {
    id: "bypass_permissions",
    legacyId: "BypassPermissions",
    name: "Yolo",
    description: "Skip all approvals (destructive)",
  },
];

export interface ModeOption {
  id: string;
  name: string;
  description: string;
}

/** `kind` selects the write path; `pendingId` is the in-flight value on the config channel. */
export type ModeChannel =
  | {
      kind: "config";
      configId: string;
      modes: ModeOption[];
      activeId: string;
      pendingId: string | null;
      label: string;
    }
  | {
      kind: "legacy";
      configId: null;
      modes: ModeOption[];
      activeId: string;
      pendingId: null;
      label: string;
    };

export interface ResolveModeChannelArgs {
  configOptions: AcpState["configOptions"];
  availableModes: AcpState["availableModes"];
  currentModeId: string | null;
  legacyMode: SessionMode;
  pendingConfigOption: AcpState["pendingConfigOption"];
  allowLegacyFallback: boolean;
}

function findModeConfig(options: ConfigOptionDescriptor[]): ConfigOptionDescriptor | undefined {
  return options.find((o) => o.category === "mode" && o.options.length > 0);
}

/** Null when the picker should not render. */
export function resolveModeChannel(args: ResolveModeChannelArgs): ModeChannel | null {
  const { configOptions, availableModes, currentModeId, legacyMode, pendingConfigOption, allowLegacyFallback } = args;

  const modeConfig = findModeConfig(configOptions);
  if (modeConfig) {
    return {
      kind: "config",
      configId: modeConfig.id,
      modes: modeConfig.options.map((o) => ({
        id: o.value,
        name: o.name,
        description: o.description ?? "",
      })),
      activeId: modeConfig.current_value,
      pendingId: pendingConfigOption?.configId === modeConfig.id ? pendingConfigOption.value : null,
      label: modeConfig.name || "Agent modes",
    };
  }

  if (availableModes.length > 0) {
    return {
      kind: "legacy",
      configId: null,
      modes: availableModes.map((m) => ({
        id: m.id,
        name: m.name,
        description: m.description ?? "",
      })),
      activeId: currentModeId ?? availableModes[0]!.id,
      pendingId: null,
      label: "Agent modes",
    };
  }

  if (allowLegacyFallback) {
    const fallbackId = LEGACY_MODES.find((m) => m.legacyId === legacyMode)?.id ?? "default";
    return {
      kind: "legacy",
      configId: null,
      modes: LEGACY_MODES.map((m) => ({
        id: m.id,
        name: m.name,
        description: m.description,
      })),
      activeId: currentModeId ?? fallbackId,
      pendingId: null,
      label: "Modes",
    };
  }

  return null;
}
