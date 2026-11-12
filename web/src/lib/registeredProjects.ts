import type { ProjectInfo, RepoGroup } from "./types";
import type { RepoColor } from "./repoAppearance";
import { workspaceIsSunk } from "./sidebarSort";
import { MULTI_REPO_GROUP_ID, SCRATCH_GROUP_ID } from "../hooks/useRepoGroups";

// Trims trailing slashes without lowercasing, which would fold distinct repos on case-sensitive filesystems.
export function normalizeProjectPathKey(path: string): string {
  return path.trim().replace(/[\\/]+$/g, "");
}

/** The registered project's per-project overrides for `path`, if it is registered. */
export function overridesForPath(projects: ProjectInfo[], path: string): ProjectInfo["overrides"] {
  const key = normalizeProjectPathKey(path);
  return projects.find((p) => normalizeProjectPathKey(p.path) === key)?.overrides;
}

function isSyntheticRepoGroup(id: string): boolean {
  return id === MULTI_REPO_GROUP_ID || id === SCRATCH_GROUP_ID;
}

// Attach registrations to repo groups by path and add an empty group per pinned project lacking one. Synthetic buckets never pin.
export function mergeRegisteredProjects(
  repoGroups: RepoGroup[],
  projects: ProjectInfo[],
  // Per-browser appearance and collapse for the appended groups.
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
    // Only a pinned registration earns a sessionless header.
    if (!registrations.some((p) => p.pinned)) continue;
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
      remoteOwnerKey: null,
      workspaces: [],
      status: "idle",
      collapsed: resolve?.collapsed(primary.path) ?? false,
      registeredProjects: registrations,
    });
  }

  return merged;
}

/**
 * The registered Projects the sidebar's Projects section lists, one per path. Where the axis draws repository
 * headers, a Project with a live group or a pin already has one, so only unpinned Projects without one are
 * listed. Otherwise (By group) every registered Project is, since it has nowhere else to appear (#5065).
 */
export function unpinnedSavedProjects(
  repoGroups: RepoGroup[],
  projects: ProjectInfo[],
  resolve?: {
    alias: (repoPath: string) => string | null;
    color: (repoPath: string) => RepoColor | null;
  },
  { drawsRepoHeaders = true }: { drawsRepoHeaders?: boolean } = {},
): RepoGroup[] {
  // An all-sunk repo does not render above, so its saved project still surfaces here.
  const livePaths = new Set<string>();
  for (const group of repoGroups) {
    if (isSyntheticRepoGroup(group.id)) continue;
    if (group.workspaces.some((ws) => !workspaceIsSunk(ws))) {
      livePaths.add(normalizeProjectPathKey(group.repoPath));
    }
  }

  const byKey = new Map<string, ProjectInfo[]>();
  for (const project of projects) {
    const key = normalizeProjectPathKey(project.path);
    if (!key) continue;
    const list = byKey.get(key);
    if (list) list.push(project);
    else byKey.set(key, [project]);
  }

  const out: RepoGroup[] = [];
  for (const [key, registrations] of byKey) {
    if (drawsRepoHeaders && livePaths.has(key)) continue; // shown above as a live group
    if (drawsRepoHeaders && registrations.some((p) => p.pinned)) continue; // shown above as a pinned header
    const primary = registrations[0];
    if (!primary) continue;
    const defaultDisplayName = primary.path.split("/").pop() || primary.path;
    const alias = resolve?.alias(primary.path) ?? null;
    out.push({
      id: primary.path,
      repoPath: primary.path,
      displayName: alias ?? defaultDisplayName,
      defaultDisplayName,
      alias,
      color: resolve?.color(primary.path) ?? null,
      remoteOwner: null,
      remoteOwnerKey: null,
      workspaces: [],
      status: "idle",
      collapsed: false,
      registeredProjects: registrations,
    });
  }
  out.sort((a, b) => a.displayName.localeCompare(b.displayName));
  return out;
}
