// Whether the backend is reachable, driven by the `/api/sessions` poller; lets the fetch interceptor suppress toast floods.

import { useEffect, useState } from "react";

let serverDown = false;
const listeners = new Set<(down: boolean) => void>();

export function setServerDown(down: boolean): void {
  if (serverDown === down) return;
  serverDown = down;
  for (const fn of listeners) fn(down);
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
