import { useMemo } from "react";
import type { SessionResponse, Workspace } from "../lib/types";
import { isSessionActive } from "../lib/session";
import { useIdleDecayWindowMs } from "../lib/idleDecay";

/** Strip trailing slashes for consistent grouping */
function normalizePath(p: string): string {
  return p.replace(/\/+$/, "");
}

// Sort order is intentionally not applied here: every consumer either
// looks up workspaces by id (App.tsx uses `.find`) or hands the list to
// `useRepoGroups`, which sorts via the shared comparator in
// `lib/workspaceSort.ts`. Keeping a second sort site is what produced the
// reshuffle bug in #1169.
export function useWorkspaces(sessions: SessionResponse[]): Workspace[] {
  const idleDecayWindowMs = useIdleDecayWindowMs();

  return useMemo(() => {
    const groups = new Map<string, SessionResponse[]>();

    // Sessions with a non-null `branch` represent a worktree and collapse
    // into a single workspace row (one row per worktree). Sessions with a
    // null `branch` (no `--worktree`) each get their own workspace; without
    // this split, multiple `aoe add <same-path>` sessions vanished behind
    // `workspace.sessions[0]`. See #956.
    for (const session of sessions) {
      const repoPath = normalizePath(session.main_repo_path ?? session.project_path);
      const key = session.branch ? `${repoPath}::${session.branch}` : `${repoPath}::__session__::${session.id}`;
      const existing = groups.get(key);
      if (existing) {
        existing.push(session);
      } else {
        groups.set(key, [session]);
      }
    }

    const workspaces: Workspace[] = [];

    for (const [id, groupSessions] of groups) {
      const first = groupSessions[0]!;
      const agents = [...new Set(groupSessions.map((s) => s.tool))];
      const status = groupSessions.some((s) => isSessionActive(s, idleDecayWindowMs)) ? "active" : "idle";

      const branch = first.branch;
      const projectPath = normalizePath(first.main_repo_path ?? first.project_path);
      const title = first.title.trim();
      const projectName = projectPath.split("/").pop() ?? projectPath;
      const displayName = groupSessions.length === 1 ? title || branch || projectName : branch || projectName;

      workspaces.push({
        id,
        branch,
        projectPath,
        displayName,
        agents,
        primaryAgent: agents[0] ?? "",
        status,
        sessions: groupSessions,
      });
    }

    return workspaces;
  }, [idleDecayWindowMs, sessions]);
}
