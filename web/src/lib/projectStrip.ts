import type { RepoGroup } from "./types";

export function matchesProjectStripFilter(group: RepoGroup, query: string) {
  if (!query) return true;
  const q = query.toLowerCase();
  return (
    group.displayName.toLowerCase().includes(q) ||
    group.defaultDisplayName.toLowerCase().includes(q) ||
    group.repoPath.toLowerCase().includes(q) ||
    group.remoteOwner?.toLowerCase().includes(q) ||
    group.workspaces.some((workspace) =>
      [
        workspace.displayName,
        workspace.branch ?? "",
        workspace.projectPath,
        workspace.primaryAgent,
        ...workspace.agents,
        ...workspace.sessions.flatMap((session) => [
          session.title,
          session.tool,
          session.status,
          session.branch ?? "",
          session.project_path,
        ]),
      ].some((value) => value.toLowerCase().includes(q)),
    )
  );
}
