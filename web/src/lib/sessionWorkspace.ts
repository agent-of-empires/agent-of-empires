import type { SessionResponse, Workspace } from "./types";

/** One session of a shared worktree as its own workspace, so its triage overlay, menu and selection never touch the group's. */
export function sessionWorkspace(group: Workspace, session: SessionResponse): Workspace {
  return { ...group, id: `${group.id}::session::${session.id}`, sessions: [session] };
}

/**
 * Per-session workspaces of every slice, once per id, for reconciling their optimistic overrides.
 * Pass every slice: the group axis splits one workspace across groups under a single id, and a later
 * slice's children would otherwise have nothing to reconcile against. Single-session slices count too,
 * so an override written while a session had a sibling keeps reconciling after the sibling is gone.
 */
export function sessionWorkspaces(slices: readonly Workspace[]): Workspace[] {
  const byId = new Map<string, Workspace>();
  for (const slice of slices) {
    for (const session of slice.sessions) {
      const child = sessionWorkspace(slice, session);
      if (!byId.has(child.id)) byId.set(child.id, child);
    }
  }
  return [...byId.values()];
}
