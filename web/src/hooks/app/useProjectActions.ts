// Pin, unpin, add, edit, and remove registered projects from the sidebar.

import { useCallback, useState } from "react";
import { createProject, deleteProject, projectTarget, updateProject } from "../../lib/api";
import { normalizeProjectPathKey } from "../../lib/registeredProjects";
import type { SidebarGroup } from "../../lib/sidebarGroups";
import { toastBus } from "../../lib/toastBus";
import type { ProjectInfo, RepoGroup } from "../../lib/types";

type Result = { ok: boolean; error?: string };

export function useProjectActions(
  projects: ProjectInfo[],
  profile: string,
  refreshProjects: () => Promise<void> | void,
) {
  const [projectForm, setProjectForm] = useState<{ editProject: ProjectInfo | null; profile: string } | null>(null);

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
          ? await Promise.all(
              existing.map((project) =>
                updateProject(project.path, projectTarget(project.scope, profile), { pinned: true }),
              ),
            )
          : [await createProject({ path: repoPath, scope: "global", profile, pinned: true })];
      const failed = results.find((r) => !r.ok);
      // Unlike unpin and remove, a failed pin skips the refresh.
      if (failed) {
        toastBus.handler?.error(failed.error ?? "Failed to pin project");
        return;
      }
      await refreshProjects();
    },
    [profile, projects, refreshProjects],
  );

  const unpinProject = useCallback(
    async (group: SidebarGroup) => {
      const pinned = group.registeredProjects.filter((p) => p.pinned);
      await finish(
        await Promise.all(
          pinned.map((project) =>
            updateProject(project.path, projectTarget(project.scope, profile), { pinned: false }),
          ),
        ),
        "Failed to unpin project",
      );
    },
    [finish, profile],
  );

  const removeProject = useCallback(
    async (group: RepoGroup) => {
      if (!confirm(`Remove project '${group.displayName}' from the sidebar?`)) return;
      await finish(
        await Promise.all(
          group.registeredProjects.map((project) => deleteProject(project.path, projectTarget(project.scope, profile))),
        ),
        "Failed to remove project",
      );
    },
    [finish, profile],
  );

  return {
    projectForm,
    closeProjectForm: useCallback(() => setProjectForm(null), []),
    addProject: useCallback(() => setProjectForm({ editProject: null, profile }), [profile]),
    editProject: useCallback((project: ProjectInfo) => setProjectForm({ editProject: project, profile }), [profile]),
    pinProject,
    unpinProject,
    removeProject,
  };
}
