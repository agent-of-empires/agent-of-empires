import type { ProjectInfo, RepoGroup } from "./types";
import type { RepoColor } from "./repoAppearance";
import { MULTI_REPO_GROUP_ID, SCRATCH_GROUP_ID } from "../hooks/useRepoGroups";

// Best-effort path key for matching a session-derived repo group against a
// registered project. The backend canonicalizes paths when a project is
// added, but the web only has the raw `Workspace.projectPath` and
// `ProjectInfo.path` strings, so this just trims and strips trailing
// slashes (NO lowercasing: that would wrongly fold distinct repos on a
// case-sensitive filesystem). A symlink / case mismatch the backend would
// resolve can still slip a duplicate header through; the real fix is a
// server endpoint returning the canonical key (tracked as follow-up), but
// session and registry paths share the same backend derivation so an exact
// match is the common case. See #2047.
export function normalizeProjectPathKey(path: string): string {
  return path.trim().replace(/[\\/]+$/g, "");
}

function isSyntheticRepoGroup(id: string): boolean {
  return id === MULTI_REPO_GROUP_ID || id === SCRATCH_GROUP_ID;
}

// Attach registry metadata to the session-derived repo groups and append a
// zero-workspace group for every registered project that has no live group
// (a pinned-but-empty project). Pure so it can be unit-tested directly and
// memoized in `useRepoGroups`.
//
// A registered path that matches a populated group attaches to it (so the
// group renders a pin marker) instead of producing a second empty header.
// Registrations are grouped by normalized path, so the same repo registered
// under both global and profile scope collapses into one group carrying both
// (unpin removes every registration for the path). Synthetic Multi-repo /
// Scratch buckets never pin: their `repoPath` is a sentinel, not a repo.
export function mergeRegisteredProjects(
  repoGroups: RepoGroup[],
  projects: ProjectInfo[],
  // Per-browser appearance/collapse for the appended empty groups, so a
  // repo that was aliased / colored / collapsed while it had sessions keeps
  // that look once it empties out and only the pin keeps it visible. Omitted
  // in unit tests, where the structural shape is what matters.
  resolve?: {
    alias: (repoPath: string) => string | null;
    color: (repoPath: string) => RepoColor | null;
    collapsed: (repoPath: string) => boolean;
  },
): RepoGroup[] {
  const byKey = new Map<string, ProjectInfo[]>();
  for (const project of projects) {
    const key = normalizeProjectPathKey(project.path);
    if (!key) continue;
    const list = byKey.get(key);
    if (list) list.push(project);
    else byKey.set(key, [project]);
  }

  const seen = new Set<string>();
  const merged = repoGroups.map((group) => {
    if (isSyntheticRepoGroup(group.id)) {
      return { ...group, registeredProjects: [] };
    }
    const key = normalizeProjectPathKey(group.repoPath);
    seen.add(key);
    return { ...group, registeredProjects: byKey.get(key) ?? [] };
  });

  for (const [key, registrations] of byKey) {
    if (seen.has(key)) continue;
    const primary = registrations[0];
    if (!primary) continue;
    const defaultDisplayName = primary.path.split("/").pop() || primary.path;
    const alias = resolve?.alias(primary.path) ?? null;
    merged.push({
      id: primary.path,
      repoPath: primary.path,
      displayName: alias ?? defaultDisplayName,
      defaultDisplayName,
      alias,
      color: resolve?.color(primary.path) ?? null,
      remoteOwner: null,
      workspaces: [],
      status: "idle",
      collapsed: resolve?.collapsed(primary.path) ?? false,
      registeredProjects: registrations,
    });
  }

  return merged;
}
