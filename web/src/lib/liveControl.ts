import { useMemo, useSyncExternalStore } from "react";

/** What a mounted live terminal pane tells the rest of the dashboard about control. */
export interface LivePaneControl {
  sessionId: string;
  surface: "agent" | "paired-host" | "paired-container";
  terminalIndex: number;
  /** Connected without control, so its keys aren't sent. */
  viewOnly: boolean;
  claim: () => void;
  /** This pane holds control on the slow stream: the top bar offers a reconnect. */
  slow: { reconnect: () => void; dismiss: () => void } | null;
}

let panes = new Map<string, LivePaneControl>();
let snapshot: readonly LivePaneControl[] = [];
const listeners = new Set<() => void>();

export function publishLivePane(key: string, pane: LivePaneControl | null): void {
  const next = new Map(panes);
  if (pane) next.set(key, pane);
  else if (!next.delete(key)) return;
  panes = next;
  snapshot = [...next.values()];
  for (const listener of listeners) listener();
}

function subscribe(listener: () => void): () => void {
  listeners.add(listener);
  return () => listeners.delete(listener);
}

const getSnapshot = () => snapshot;

/** The live panes mounted for `sessionId`, agent first. */
export function useLivePanes(sessionId: string | null): LivePaneControl[] {
  const all = useSyncExternalStore(subscribe, getSnapshot, getSnapshot);
  return useMemo(
    () =>
      all
        .filter((p) => p.sessionId === sessionId)
        .sort((a, b) => Number(b.surface === "agent") - Number(a.surface === "agent")),
    [all, sessionId],
  );
}
