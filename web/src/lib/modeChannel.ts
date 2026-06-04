// Mode-picker channel resolution (#1764).
//
// The structured view composer's mode picker can be driven by three different
// sources depending on what the active agent advertises over ACP. This
// module owns the precedence and the read/write pairing so the picker
// never reads one channel while writing another (the bug that trapped
// OpenCode users: OpenCode advertises modes via the config-option
// channel and rejects session/set_mode, so reading config + writing
// set_mode left them with a phantom "default" mode they could not
// leave).
//
// Precedence, most authoritative first:
//   1. config: an ACP config option of category "mode" (OpenCode, and
//      claude-agent-acp v0.37.0+). Active value is `current_value`;
//      switches go through session/set_config_option.
//   2. legacy: ACP SessionModeState (`availableModes` / `currentModeId`,
//      older claude). Switches go through session/set_mode.
//   3. fallback: claude's hardcoded Default/Plan/AcceptEdits/Yolo
//      taxonomy, only when the agent's profile opts in
//      (`capabilities.legacyModeFallback`). Switches go through
//      session/set_mode.
// When none apply (a non-claude agent that advertised nothing), the
// picker renders nothing rather than a vocabulary the agent rejects.

import type {
  AcpState,
  ConfigOptionDescriptor,
  SessionMode,
} from "./acpTypes";

/** Claude's historical four-mode taxonomy. Used only as the
 *  `capabilities.legacyModeFallback` fallback; not an ACP default. */
export const LEGACY_MODES: ReadonlyArray<{
  id: string;
  legacyId: SessionMode;
  name: string;
  description: string;
}> = [
  { id: "default", legacyId: "Default", name: "Default", description: "Approve each tool individually" },
  { id: "plan", legacyId: "Plan", name: "Plan", description: "Plan first, no edits applied" },
  { id: "accept_edits", legacyId: "AcceptEdits", name: "Accept edits", description: "Auto-approve safe file edits" },
  { id: "bypass_permissions", legacyId: "BypassPermissions", name: "Yolo", description: "Skip all approvals (destructive)" },
];

export interface ModeOption {
  id: string;
  name: string;
  description: string;
}

/** The resolved channel the picker should read and write. `kind`
 *  selects the write path: "config" -> session/set_config_option on
 *  `configId`; "legacy" -> session/set_mode. `pendingId` is the value
 *  currently in flight (config channel only; pessimistic UI). A
 *  discriminated union so the "config" variant guarantees a non-null
 *  `configId`. */
export type ModeChannel =
  | {
      kind: "config";
      configId: string;
      modes: ModeOption[];
      activeId: string;
      pendingId: string | null;
      /** Menu header label. */
      label: string;
    }
  | {
      kind: "legacy";
      configId: null;
      modes: ModeOption[];
      activeId: string;
      pendingId: null;
      /** Menu header label. */
      label: string;
    };

export interface ResolveModeChannelArgs {
  configOptions: AcpState["configOptions"];
  availableModes: AcpState["availableModes"];
  currentModeId: string | null;
  legacyMode: SessionMode;
  pendingConfigOption: AcpState["pendingConfigOption"];
  /** From the active agent's profile (`capabilities.legacyModeFallback`). */
  allowLegacyFallback: boolean;
}

function findModeConfig(
  options: ConfigOptionDescriptor[],
): ConfigOptionDescriptor | undefined {
  return options.find(
    (o) => o.category === "mode" && o.options.length > 0,
  );
}

/** Resolve the mode-picker channel, or null when the picker should not
 *  render. Pure; unit-tested in `modeChannel.test.ts`. */
export function resolveModeChannel(
  args: ResolveModeChannelArgs,
): ModeChannel | null {
  const {
    configOptions,
    availableModes,
    currentModeId,
    legacyMode,
    pendingConfigOption,
    allowLegacyFallback,
  } = args;

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
      pendingId:
        pendingConfigOption?.configId === modeConfig.id
          ? pendingConfigOption.value
          : null,
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
    const fallbackId =
      LEGACY_MODES.find((m) => m.legacyId === legacyMode)?.id ?? "default";
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
