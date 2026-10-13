import { useCallback, useLayoutEffect, useSyncExternalStore } from "react";

import { switchAcpProvider } from "../../lib/api";
import { reportError } from "../../lib/toastBus";

type ProviderEcho = { value: string; from: string | null };
type Snapshot = { pending: string | null; echo: ProviderEcho | null };
type Entry = { snapshot: Snapshot; listeners: Set<() => void>; requestFrom: string | null; canonicalMoved: boolean };
const EMPTY: Snapshot = { pending: null, echo: null };
const requests = new Map<string, Entry>();

function entryFor(sessionId: string): Entry {
  let entry = requests.get(sessionId);
  if (!entry) {
    entry = { snapshot: EMPTY, listeners: new Set(), requestFrom: null, canonicalMoved: false };
    requests.set(sessionId, entry);
  }
  return entry;
}

function releaseUnused(sessionId: string, entry: Entry) {
  if (entry.listeners.size === 0 && entry.snapshot.pending === null) requests.delete(sessionId);
}

function publish(sessionId: string, entry: Entry, snapshot: Snapshot) {
  entry.snapshot = snapshot;
  for (const listener of entry.listeners) listener();
  releaseUnused(sessionId, entry);
}

/** Requests outlive keyed views; completed echoes retire when the canonical row changes. */
export function useProviderSwitch(sessionId: string, agent: string | null, serverProvider: string | null) {
  const subscribe = useCallback(
    (listener: () => void) => {
      const entry = entryFor(sessionId);
      entry.listeners.add(listener);
      return () => {
        entry.listeners.delete(listener);
        releaseUnused(sessionId, entry);
      };
    },
    [sessionId],
  );
  const getSnapshot = useCallback(() => requests.get(sessionId)?.snapshot ?? EMPTY, [sessionId]);
  const snapshot = useSyncExternalStore(subscribe, getSnapshot);
  const echo = snapshot.echo && snapshot.echo.from === serverProvider ? snapshot.echo.value : null;
  useLayoutEffect(() => {
    const entry = requests.get(sessionId);
    if (entry && entry.snapshot.pending !== null && entry.requestFrom !== serverProvider) entry.canonicalMoved = true;
    if (entry?.snapshot.echo && entry.snapshot.echo.from !== serverProvider) {
      publish(sessionId, entry, { ...entry.snapshot, echo: null });
    }
  }, [sessionId, serverProvider, snapshot.echo, snapshot.pending]);

  const set = useCallback(
    async (next: string) => {
      const entry = entryFor(sessionId);
      if (entry.snapshot.pending !== null) return;
      entry.requestFrom = serverProvider;
      entry.canonicalMoved = false;
      publish(sessionId, entry, { ...entry.snapshot, pending: next });
      let accepted: ProviderEcho | undefined;
      try {
        const result = await switchAcpProvider(sessionId, next);
        if (!entry.canonicalMoved) accepted = { value: result.provider, from: serverProvider };
      } catch (e) {
        reportError(`Provider switch failed: ${e instanceof Error ? e.message : String(e)}`);
      } finally {
        publish(sessionId, entry, { pending: null, echo: accepted ?? entry.snapshot.echo });
      }
    },
    [sessionId, serverProvider],
  );

  if (agent !== "claude" && agent !== "claude-code") return { current: null, pending: null, set: undefined };
  return { current: echo ?? serverProvider, pending: snapshot.pending, set };
}
