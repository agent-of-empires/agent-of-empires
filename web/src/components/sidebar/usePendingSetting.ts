import { createContext, useContext } from "react";
import type { SessionResponse } from "../../lib/types";

export interface SettingValues {
  color: string | null;
  notifications: "off" | "default" | "all";
}

export interface SessionActionTarget {
  id: string;
  profile: string;
  createdAt: string;
  epoch: string;
}

export function sessionActionTarget(row: SessionResponse, epoch: string | null): SessionActionTarget {
  return { id: row.id, profile: row.profile, createdAt: row.created_at, epoch: epoch ?? "" };
}

export interface SessionActionsOwner {
  openAttach: (session: SessionResponse) => void;
  value: <K extends keyof SettingValues>(
    kind: K,
    targets: SessionActionTarget[],
    server: SettingValues[K],
  ) => SettingValues[K];
  set: <K extends keyof SettingValues>(
    kind: K,
    targets: SessionActionTarget[],
    next: SettingValues[K],
    server: SettingValues[K],
  ) => void;
}

export const SessionActionContext = createContext<SessionActionsOwner | null>(null);

export function useSessionActions(): SessionActionsOwner {
  const owner = useContext(SessionActionContext);
  if (!owner) throw new Error("Session actions require their App-owned provider");
  return owner;
}

/** Visual rows bind to accepted work; unmounting a row does not revoke its targets. */
export function usePendingSetting<K extends keyof SettingValues>(
  kind: K,
  targets: SessionActionTarget[],
  server: SettingValues[K],
) {
  const owner = useSessionActions();
  return [
    owner.value(kind, targets, server),
    (next: SettingValues[K]) => owner.set(kind, targets, next, server),
  ] as const;
}
