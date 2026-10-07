import { memo, useRef } from "react";
import type { ProjectInfo, RepoGroup } from "../lib/types";
import { repoColorStyle } from "../lib/repoAppearance";
import { useSidebarCompact } from "../lib/sidebarCompact";
import { ContextMenu, MenuItem, MenuSeparator } from "./ContextMenu";
import { FoldChevron, PlusIcon } from "./icons";
import { useContextMenu } from "./useContextMenu";
import { useLongPress } from "./sidebar/useLongPress";
import { usePersistedFlag } from "./usePersistedFlag";

interface ProjectsSectionProps {
  // No-session registered projects, one entry per path.
  projects: RepoGroup[];
  // Lowercased, trimmed filter query; empty means no filter.
  query: string;
  readOnly?: boolean;
  offline: boolean;
  onCreateSession: (repoPath: string) => void;
  onAddProject: () => void;
  onEditProject: (project: ProjectInfo) => void;
  onRemoveProject: (group: RepoGroup) => void;
}

export function ProjectsSection({
  projects,
  query,
  readOnly,
  offline,
  onCreateSession,
  onAddProject,
  onEditProject,
  onRemoveProject,
}: ProjectsSectionProps) {
  const [expanded, toggle] = usePersistedFlag("aoe-projects-section-expanded", true);
  const compact = useSidebarCompact();

  const visible = query
    ? projects.filter((p) => p.displayName.toLowerCase().includes(query) || p.repoPath.toLowerCase().includes(query))
    : projects;

  // With CRUD available the header stays so Add is reachable with zero projects.
  const canAdd = !readOnly && !offline;
  if (visible.length === 0 && !canAdd) return null;

  return (
    <div data-testid="sidebar-projects-section">
      <div className="w-full flex items-center gap-2 border-t border-surface-800/60">
        <button
          onClick={toggle}
          data-testid="sidebar-projects-toggle"
          aria-expanded={expanded}
          className={`flex-1 min-w-0 flex items-center gap-2 py-1.5 text-[11px] font-mono uppercase text-text-muted hover:text-text-secondary hover:bg-surface-800/40 cursor-pointer transition-colors ${
            compact ? "px-2" : "px-3 tracking-widest"
          }`}
        >
          <FoldChevron className={`shrink-0 transition-transform duration-75 ${expanded ? "" : "-rotate-90"}`} />
          {/* The count and the wide tracking do not fit the rail, and a clipped
              "(0)" is exactly what looks broken; truncate the label. See #2288. */}
          <span className="truncate">Projects{compact ? "" : ` (${visible.length})`}</span>
        </button>
        {canAdd && !compact && (
          <button
            onClick={onAddProject}
            data-testid="sidebar-projects-add"
            title="Add project"
            aria-label="Add project"
            className="shrink-0 w-7 h-7 mr-1 flex items-center justify-center text-text-muted hover:text-text-secondary hover:bg-surface-800 cursor-pointer rounded-md transition-colors"
          >
            <PlusIcon size={14} />
          </button>
        )}
      </div>
      {expanded &&
        visible.map((project) => (
          <ProjectRow
            key={project.repoPath}
            project={project}
            readOnly={readOnly}
            offline={offline}
            onCreateSession={onCreateSession}
            onEditProject={onEditProject}
            onRemoveProject={onRemoveProject}
          />
        ))}
      {expanded && visible.length === 0 && !compact && (
        <p className="px-3 py-2 text-[12px] text-text-dim">
          {query ? "No matching projects." : "No saved projects. Add one to keep a repo handy without a session."}
        </p>
      )}
    </div>
  );
}

const ProjectRow = memo(function ProjectRow({
  project,
  readOnly,
  offline,
  onCreateSession,
  onEditProject,
  onRemoveProject,
}: {
  project: RepoGroup;
  readOnly?: boolean;
  offline: boolean;
  onCreateSession: (repoPath: string) => void;
  onEditProject: (project: ProjectInfo) => void;
  onRemoveProject: (group: RepoGroup) => void;
}) {
  const openedAtRef = useRef(0);
  const { menu, menuRef, openMenu, closeMenu } = useContextMenu<{ x: number; y: number }>(openedAtRef);
  const baseBranch = project.registeredProjects[0]?.default_base_branch;
  const canModify = !readOnly && !offline;
  const longPress = useLongPress(canModify, (x, y) => {
    openedAtRef.current = Date.now();
    openMenu({ x, y });
  });

  return (
    <>
      <div
        data-testid="sidebar-project-row"
        data-repo-path={project.repoPath}
        tabIndex={canModify ? 0 : undefined}
        aria-haspopup={canModify ? "menu" : undefined}
        aria-label={canModify ? `Project actions for ${project.displayName}` : undefined}
        onContextMenu={
          canModify
            ? (e) => {
                e.preventDefault();
                openMenu({ x: e.clientX, y: e.clientY });
              }
            : undefined
        }
        {...longPress.handlers}
        onKeyDown={
          canModify
            ? (e) => {
                // Only the row itself, not the inner New-session button.
                if (e.target !== e.currentTarget) return;
                if (e.key !== "ContextMenu" && !(e.shiftKey && e.key === "F10")) return;
                e.preventDefault();
                const rect = e.currentTarget.getBoundingClientRect();
                openMenu({ x: rect.left + 12, y: rect.bottom + 4 });
              }
            : undefined
        }
        className="group flex items-center gap-2 px-3 py-1.5 text-text-secondary hover:bg-surface-800/50 transition-colors focus:outline-none focus:ring-2 focus:ring-brand-600 select-none [-webkit-touch-callout:none]"
        style={repoColorStyle(project.color)}
      >
        <span className="shrink-0 text-[10px] leading-none text-text-dim" title="Saved project" aria-hidden>
          ◆
        </span>
        <button
          onClick={() => canModify && onCreateSession(project.repoPath)}
          disabled={!canModify}
          title={`New session in ${project.displayName}`}
          className="min-w-0 flex-1 text-left cursor-pointer disabled:cursor-not-allowed"
        >
          <span className="block truncate text-[13px] md:text-[14px] font-mono text-text-primary">
            {project.displayName}
          </span>
          <span className="block truncate text-[11px] text-text-dim" title={project.repoPath}>
            {project.repoPath}
            {baseBranch ? ` · ${baseBranch}` : ""}
          </span>
        </button>
        {canModify && (
          <button
            onClick={() => onCreateSession(project.repoPath)}
            title="New session"
            aria-label={`New session in ${project.displayName}`}
            className="shrink-0 w-6 h-6 flex items-center justify-center text-text-dim opacity-0 group-hover:opacity-100 hover:text-text-primary hover:bg-surface-700/50 cursor-pointer rounded transition"
          >
            <PlusIcon size={13} />
          </button>
        )}
      </div>

      {canModify && menu && (
        <ContextMenu menu={menu} menuRef={menuRef} testId="sidebar-project-context-menu">
          {project.registeredProjects.map((reg) => (
            <MenuItem
              key={`${reg.scope}:${reg.path}`}
              onClick={() => {
                closeMenu();
                onEditProject(reg);
              }}
              testId="sidebar-project-context-menu-edit"
            >
              {project.registeredProjects.length > 1 ? `Project settings (${reg.scope})` : "Project settings"}
            </MenuItem>
          ))}
          <MenuSeparator />
          <MenuItem
            onClick={() => {
              closeMenu();
              onRemoveProject(project);
            }}
            testId="sidebar-project-context-menu-remove"
          >
            Remove project
          </MenuItem>
        </ContextMenu>
      )}
    </>
  );
});
