import { isSessionActive } from "./session";
import { workspaceIsPinned, workspaceIsSunk } from "./sidebarSort";
import type { SessionResponse, Workspace } from "./types";

export function sessionMatchesActivity(s: SessionResponse, idleDecayWindowMs: number): boolean {
  return (
    !!s.favorited ||
    s.pinned_at != null ||
    isSessionActive(s, idleDecayWindowMs) ||
    s.status === "Creating" ||
    s.status === "Error"
  );
}

/** Visibility only: anchors and recovery controls remain reachable without
 *  changing a session's lifecycle or the current terminal. */
export function workspaceMatchesActivity(ws: Workspace, selectedId: string | null, idleDecayWindowMs: number): boolean {
  return (
    workspaceIsSunk(ws) ||
    ws.id === selectedId ||
    workspaceIsPinned(ws) ||
    ws.sessions.some((s) => sessionMatchesActivity(s, idleDecayWindowMs))
  );
}
