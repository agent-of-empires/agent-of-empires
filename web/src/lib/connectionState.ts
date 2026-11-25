// Whether the backend is reachable, driven by the `/api/sessions` poller; lets the fetch interceptor suppress toast floods.

import { useEffect, useState } from "react";

let serverDown = false;
const listeners = new Set<(down: boolean) => void>();

export function setServerDown(down: boolean): void {
  if (serverDown === down) return;
  serverDown = down;
  for (const fn of listeners) fn(down);
  if (!down) noteDaemonChanged();
}

const changeListeners = new Set<() => void>();

/** The Daemon may have changed under the page: it reconnected, or a read reached a new Daemon epoch. A restart
 *  between two polls shows only as the epoch, so callers that re-read server settings listen here. */
export function onDaemonChanged(fn: () => void): () => void {
  changeListeners.add(fn);
  return () => changeListeners.delete(fn);
}

export function noteDaemonChanged(): void {
  for (const fn of changeListeners) fn();
}

export function isServerDown(): boolean {
  return serverDown;
}

export function onServerDownChange(fn: (down: boolean) => void): () => void {
  listeners.add(fn);
  return () => listeners.delete(fn);
}

/** Lets controls that need the API disable themselves without prop drilling. */
export function useServerDown(): boolean {
  const [down, setDown] = useState<boolean>(serverDown);
  useEffect(() => onServerDownChange(setDown), []);
  return down;
}

export const OFFLINE_TITLE = "Disconnected — reconnect to use";
