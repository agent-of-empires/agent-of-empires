import type { Workspace } from "../../lib/types";
import type { RuntimeCursor, SessionMutation } from "../../lib/api";
import { useIdleDecayWindowMs } from "../../lib/idleDecay";
import { useUnreadIndicatorEnabled } from "../../lib/unreadIndicator";
import { usePluginAttention } from "../../lib/pluginUiContext";
import type { OptimisticTriage } from "../../lib/sidebarOptimistic";
import { deriveRowModel, SESSION_COLOR_OPTIONS, type RowModel } from "./rowModel";
import { sessionActionTarget, usePendingSetting } from "./usePendingSetting";

export interface RowObservation {
  observedById: Record<string, RuntimeCursor>;
  runtimeEpoch: string | null;
  onSessionMutation: (mutation: SessionMutation) => void;
}

/** The row model with its notify and color picks applied optimistically. */
export function useSessionRowModel(
  workspace: Workspace,
  optimistic: OptimisticTriage,
  isActive: boolean,
  observation: RowObservation,
) {
  const idleDecayWindowMs = useIdleDecayWindowMs();
  const unreadIndicatorEnabled = useUnreadIndicatorEnabled();
  const pluginAttention = usePluginAttention();
  const derived = deriveRowModel(workspace, optimistic, {
    idleDecayWindowMs,
    isActive,
    unreadIndicatorEnabled,
    pluginAttention,
  });
  const targets = workspace.sessions.map((session) => sessionActionTarget(session, observation.runtimeEpoch));
  const [notifyPreset, setNotify] = usePendingSetting("notifications", targets.slice(0, 1), derived.notifyPreset);
  const [sessionColor, setColor] = usePendingSetting("color", targets, derived.sessionColor);
  const model: RowModel = {
    ...derived,
    notifyPreset,
    sessionColor,
    sessionColorDot: SESSION_COLOR_OPTIONS.find((o) => o.key === sessionColor)?.dotClass ?? null,
  };
  return { model, setNotify, setColor };
}
