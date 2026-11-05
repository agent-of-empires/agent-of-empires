import { useEffect, useState } from "react";
import { fetchPluginPane, type PluginUiEntry } from "../lib/api";
import { usePluginUiRevision } from "../lib/pluginUiContext";

export type LazyPane =
  | { status: "loading" }
  | { status: "failed" }
  | { status: "ready"; payload: Record<string, unknown> };

/** The content of a lazy pane, refetched whenever its session's UI revision moves. Null for an eager pane. */
export function useLazyPane(entry: PluginUiEntry): LazyPane | null {
  const { plugin_id: pluginId, id, session_id: sessionId, lazy } = entry;
  const revision = usePluginUiRevision(pluginId, sessionId);
  const [state, setState] = useState<{ key: string; pane: LazyPane }>({ key: "", pane: { status: "loading" } });
  const key = `${pluginId}/${id}/${sessionId}`;

  useEffect(() => {
    if (!lazy || !sessionId) return;
    const controller = new AbortController();
    void fetchPluginPane(pluginId, id, sessionId, controller.signal).then((payload) => {
      if (controller.signal.aborted) return;
      setState((prev) => ({
        key,
        pane: payload
          ? { status: "ready", payload }
          : prev.key === key && prev.pane.status === "ready"
            ? prev.pane
            : { status: "failed" },
      }));
    });
    return () => controller.abort();
  }, [lazy, pluginId, id, sessionId, key, revision]);

  if (!lazy) return null;
  return state.key === key ? state.pane : { status: "loading" };
}
