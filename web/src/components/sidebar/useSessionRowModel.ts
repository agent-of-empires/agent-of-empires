import type { Workspace } from "../../lib/types";
import { setSessionColor, setSessionNotifications } from "../../lib/api";
import { useIdleDecayWindowMs } from "../../lib/idleDecay";
import { useUnreadIndicatorEnabled } from "../../lib/unreadIndicator";
import { usePluginAttention } from "../../lib/pluginUiContext";
import type { OptimisticTriage } from "../../lib/sidebarOptimistic";
import { reportError } from "../../lib/toastBus";
import { deriveRowModel, SESSION_COLOR_OPTIONS, type RowModel } from "./rowModel";
import { usePendingSetting } from "./usePendingSetting";

/** The row model with its notify and color picks applied optimistically. */
export function useSessionRowModel(workspace: Workspace, optimistic: OptimisticTriage, isActive: boolean) {
  const idleDecayWindowMs = useIdleDecayWindowMs();
  const unreadIndicatorEnabled = useUnreadIndicatorEnabled();
  const pluginAttention = usePluginAttention();
  const derived = deriveRowModel(workspace, optimistic, {
    idleDecayWindowMs,
    isActive,
    unreadIndicatorEnabled,
    pluginAttention,
  });
  const { sessionId } = derived;
  const [notifyPreset, setNotify] = usePendingSetting(
    derived.notifyPreset,
    (preset) => (sessionId ? setSessionNotifications(sessionId, preset) : Promise.resolve(false)),
    () => reportError("Could not change notifications. Please try again."),
  );
  const [sessionColor, setColor] = usePendingSetting(
    derived.sessionColor,
    // The row shows any session's color, so every session must change for the pick to stick.
    async (color) =>
      (await Promise.all(workspace.sessions.map((s) => setSessionColor(s.id, color)))).every((r) => r != null),
    () => reportError("Could not change the session color. Please try again."),
  );
  const model: RowModel = {
    ...derived,
    notifyPreset,
    sessionColor,
    sessionColorDot: SESSION_COLOR_OPTIONS.find((o) => o.key === sessionColor)?.dotClass ?? null,
  };
  return { model, setNotify, setColor };
}
