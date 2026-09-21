import { useMemo } from "react";
import type { SessionResponse, Workspace } from "../lib/types";
import { isSessionActive } from "../lib/session";
import { useIdleDecayWindowMs } from "../lib/idleDecay";

function normalizePath(p: string): string {
  return p.replace(/\/+$/, "");
}

export function useWorkspaces(sessions: SessionResponse[]): Workspace[] {
  const idleDecayWindowMs = useIdleDecayWindowMs();

  return useMemo(() => {
    const groups = new Map<string, SessionResponse[]>();

    // Worktree sessions (non-null branch) share a row; plain sessions each get their own (#956).
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
