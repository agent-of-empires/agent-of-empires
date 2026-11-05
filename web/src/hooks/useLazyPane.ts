import { useEffect, useRef, useState } from "react";
import { fetchPluginPane, type PluginUiEntry } from "../lib/api";
import { usePluginUiGeneration, usePluginUiRevision } from "../lib/pluginUiContext";

export type LazyPane =
  | { status: "loading" }
  | { status: "failed" }
  | { status: "ready"; payload: Record<string, unknown> };

// Delays before each retry of a failed render; once spent, only a moved revision or a new generation tries again.
const RETRY_DELAYS_MS = [2_000, 5_000, 15_000];

/**
 * The content of a lazy pane. One loop per pane identity and UI generation fetches it; a moved session UI
 * revision asks the running loop for one more render instead of aborting the request in flight, so a slow render
 * still lands while revisions keep advancing. Null for an eager pane.
 */
export function useLazyPane(entry: PluginUiEntry): LazyPane | null {
  const { plugin_id: pluginId, id, session_id: sessionId, lazy } = entry;
  const revision = usePluginUiRevision(pluginId, sessionId);
  const generation = usePluginUiGeneration();
  const [state, setState] = useState<{ key: string; pane: LazyPane }>({ key: "", pane: { status: "loading" } });
  const key = `${pluginId}/${id}/${sessionId}@${generation}`;
  const latestRevision = useRef(revision);
  const loop = useRef<{ revision: number; render: () => void } | null>(null);

  useEffect(() => {
    latestRevision.current = revision;
  });

  useEffect(() => {
    if (!lazy || !sessionId) return;
    const controller = new AbortController();
    let busy = false;
    let again = false;
    let failures = 0;
    let retry: ReturnType<typeof setTimeout> | undefined;

    const render = () => {
      if (busy) {
        again = true;
        return;
      }
      busy = true;
      again = false;
      clearTimeout(retry);
      void fetchPluginPane(pluginId, id, sessionId, controller.signal).then((payload) => {
        busy = false;
        if (controller.signal.aborted) return;
        failures = payload ? 0 : failures + 1;
        setState((prev) => ({
          key,
          pane: payload
            ? { status: "ready", payload }
            : prev.key === key && prev.pane.status === "ready"
              ? prev.pane
              : { status: "failed" },
        }));
        if (again) render();
        else if (!payload && failures <= RETRY_DELAYS_MS.length)
          retry = setTimeout(render, RETRY_DELAYS_MS[failures - 1]);
      });
    };

    loop.current = { revision: latestRevision.current, render };
    render();
    return () => {
      controller.abort();
      clearTimeout(retry);
      loop.current = null;
    };
  }, [lazy, pluginId, id, sessionId, key]);

  useEffect(() => {
    if (loop.current && loop.current.revision !== revision) {
      loop.current.revision = revision;
      loop.current.render();
    }
  }, [revision]);

  if (!lazy) return null;
  return state.key === key ? state.pane : { status: "loading" };
}
