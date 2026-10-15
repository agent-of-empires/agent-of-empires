// Pin, unpin, add, edit, and remove registered projects from the sidebar.

import { useCallback, useState } from "react";
import { createProject, deleteProject, setProjectPinned } from "../../lib/api";
import { normalizeProjectPathKey } from "../../lib/registeredProjects";
import type { SidebarGroup } from "../../lib/sidebarGroups";
import { toastBus } from "../../lib/toastBus";
import type { ProjectInfo, RepoGroup } from "../../lib/types";

type Result = { ok: boolean; error?: string };

export function useProjectActions(projects: ProjectInfo[], refreshProjects: () => Promise<void> | void) {
  const [projectForm, setProjectForm] = useState<{ editProject: ProjectInfo | null } | null>(null);

  const finish = useCallback(
    async (results: Result[], fallback: string) => {
      const failed = results.find((r) => !r.ok);
      if (failed) toastBus.handler?.error(failed.error ?? fallback);
      await refreshProjects();
    },
    [refreshProjects],
  );

  const pinProject = useCallback(
    async (repoPath: string) => {
      const key = normalizeProjectPathKey(repoPath);
      const existing = projects.filter((p) => normalizeProjectPathKey(p.path) === key);
      const results =
        existing.length > 0
          ? await Promise.all(existing.map((p) => setProjectPinned(p.name, p.scope, true)))
          : [await createProject({ path: repoPath, scope: "global", pinned: true })];
      const failed = results.find((r) => !r.ok);
      // Unlike unpin and remove, a failed pin skips the refresh.
      if (failed) {
        toastBus.handler?.error(failed.error ?? "Failed to pin project");
        return;
      }
      await refreshProjects();
    },
    [projects, refreshProjects],
  );

  const unpinProject = useCallback(
    async (group: SidebarGroup) => {
      const pinned = group.registeredProjects.filter((p) => p.pinned);
      await finish(
        await Promise.all(pinned.map((p) => setProjectPinned(p.name, p.scope, false))),
        "Failed to unpin project",
      );
    },
    [finish],
  );

  const removeProject = useCallback(
    async (group: RepoGroup) => {
      if (!confirm(`Remove project '${group.displayName}' from the sidebar?`)) return;
      await finish(
        await Promise.all(group.registeredProjects.map((p) => deleteProject(p.name, p.scope))),
        "Failed to remove project",
      );
    },
    [finish],
  );

  return {
    projectForm,
    closeProjectForm: useCallback(() => setProjectForm(null), []),
    addProject: useCallback(() => setProjectForm({ editProject: null }), []),
    editProject: useCallback((project: ProjectInfo) => setProjectForm({ editProject: project }), []),
    pinProject,
    unpinProject,
    removeProject,
  };
}
