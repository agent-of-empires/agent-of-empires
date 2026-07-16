import { memo, useCallback, useEffect, useRef, useState } from "react";
import { createPortal } from "react-dom";
import type { ProjectInfo, RepoGroup } from "../lib/types";
import { repoColorStyle } from "../lib/repoAppearance";
import { menuBus, closeOtherContextMenus } from "../lib/menuBus";
import { useClampedMenuPosition } from "../lib/menuPosition";
import { safeGetItem, safeSetItem } from "../lib/safeStorage";

const EXPANDED_KEY = "aoe-projects-section-expanded";

function loadExpanded(): boolean {
  // Expanded by default; only an explicit "false" collapses it.
  return safeGetItem(EXPANDED_KEY) !== "false";
}

interface ProjectsSectionProps {
  // No-session registered projects, one entry per path (scopes collapsed),
  // carrying alias/color. Sourced from useRepoGroups().savedProjects.
  projects: RepoGroup[];
  // The active sidebar filter query (already lowercased + trimmed). Empty
  // string means no filter.
  query: string;
  readOnly?: boolean;
  offline: boolean;
  onCreateSession: (repoPath: string) => void;
  onAddProject: () => void;
  onEditProject: (project: ProjectInfo) => void;
  onRemoveProject: (group: RepoGroup) => void;
}

// Dedicated, axis-independent section listing registered projects that have no
// live session, with add / edit-base-branch / remove moved off the former
// /projects page. Sits between the session groups and the Snoozed & archived
// footer. See #2212.
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
  const [expanded, setExpanded] = useState<boolean>(loadExpanded);

  const toggle = useCallback(() => {
    setExpanded((prev) => {
      const next = !prev;
      safeSetItem(EXPANDED_KEY, next ? "true" : "false");
      return next;
    });
  }, []);

  const visible = query
    ? projects.filter((p) => p.displayName.toLowerCase().includes(query) || p.repoPath.toLowerCase().includes(query))
    : projects;

  // Hide the whole section when there is nothing to show and no way to add
  // (read-only / offline). With CRUD available, keep the header so the add
  // button stays reachable even with zero projects.
  const canAdd = !readOnly && !offline;
  if (visible.length === 0 && !canAdd) return null;

  return (
    <div data-testid="sidebar-projects-section">
      <div className="w-full flex items-center gap-2 border-t border-surface-800/60">
        <button
          onClick={toggle}
          data-testid="sidebar-projects-toggle"
          aria-expanded={expanded}
          className="flex-1 flex items-center gap-2 px-3 py-1.5 text-[11px] font-mono uppercase tracking-widest text-text-muted hover:text-text-secondary hover:bg-surface-800/40 cursor-pointer transition-colors"
        >
          <svg
            width="10"
            height="10"
            viewBox="0 0 10 10"
            fill="currentColor"
            className={`shrink-0 transition-transform duration-75 ${expanded ? "" : "-rotate-90"}`}
          >
            <path
              d="M2 3 L5 6.5 L8 3"
              fill="none"
              stroke="currentColor"
              strokeWidth="1.5"
              strokeLinecap="round"
              strokeLinejoin="round"
            />
          </svg>
          <span>Projects ({visible.length})</span>
        </button>
        {canAdd && (
          <button
            onClick={onAddProject}
            data-testid="sidebar-projects-add"
            title="Add project"
            aria-label="Add project"
            className="shrink-0 w-7 h-7 mr-1 flex items-center justify-center text-text-muted hover:text-text-secondary hover:bg-surface-800 cursor-pointer rounded-md transition-colors"
          >
            <svg
              width="14"
              height="14"
              viewBox="0 0 24 24"
              fill="none"
              stroke="currentColor"
              strokeWidth="2"
              strokeLinecap="round"
              strokeLinejoin="round"
            >
              <line x1="12" y1="5" x2="12" y2="19" />
              <line x1="5" y1="12" x2="19" y2="12" />
            </svg>
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
      {expanded && visible.length === 0 && (
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
  const [contextMenu, setContextMenu] = useState<{ x: number; y: number } | null>(null);
  const menuRef = useRef<HTMLDivElement>(null);
  const baseBranch = project.registeredProjects[0]?.default_base_branch;
  const canModify = !readOnly && !offline;
  const hasMenu = canModify;

  const openMenuAt = useCallback((x: number, y: number) => {
    closeOtherContextMenus();
    setContextMenu({ x, y });
  }, []);

  useClampedMenuPosition(contextMenu, menuRef, setContextMenu);

  useEffect(() => {
    if (!contextMenu) return;
    const close = () => setContextMenu(null);
    const onDocClick = (e: MouseEvent) => {
      if (menuRef.current?.contains(e.target as Node)) return;
      close();
    };
    const id = requestAnimationFrame(() => {
      document.addEventListener("click", onDocClick);
      document.addEventListener("contextmenu", close);
    });
    menuBus.addEventListener("close", close);
    return () => {
      cancelAnimationFrame(id);
      document.removeEventListener("click", onDocClick);
      document.removeEventListener("contextmenu", close);
      menuBus.removeEventListener("close", close);
    };
  }, [contextMenu]);

  return (
    <>
      <div
        data-testid="sidebar-project-row"
        data-repo-path={project.repoPath}
        tabIndex={hasMenu ? 0 : undefined}
        aria-haspopup={hasMenu ? "menu" : undefined}
        aria-label={hasMenu ? `Project actions for ${project.displayName}` : undefined}
        onContextMenu={
          hasMenu
            ? (e) => {
                e.preventDefault();
                openMenuAt(e.clientX, e.clientY);
              }
            : undefined
        }
        onKeyDown={
          hasMenu
            ? (e) => {
                // Keyboard path to the edit/remove menu, mirroring
                // SidebarGroupHeader. Only fire on the row itself, not the
                // inner New-session button.
                if (e.target !== e.currentTarget) return;
                if (e.key !== "ContextMenu" && !(e.shiftKey && e.key === "F10")) return;
                e.preventDefault();
                const rect = e.currentTarget.getBoundingClientRect();
                openMenuAt(rect.left + 12, rect.bottom + 4);
              }
            : undefined
        }
        className="group flex items-center gap-2 px-3 py-1.5 text-text-secondary hover:bg-surface-800/50 transition-colors focus:outline-none focus:ring-2 focus:ring-brand-600"
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
            <svg
              width="13"
              height="13"
              viewBox="0 0 24 24"
              fill="none"
              stroke="currentColor"
              strokeWidth="2"
              strokeLinecap="round"
              strokeLinejoin="round"
            >
              <line x1="12" y1="5" x2="12" y2="19" />
              <line x1="5" y1="12" x2="19" y2="12" />
            </svg>
          </button>
        )}
      </div>

      {hasMenu &&
        contextMenu &&
        createPortal(
          <div
            ref={menuRef}
            data-testid="sidebar-project-context-menu"
            className="fixed z-50 bg-surface-800 border border-surface-700 rounded-lg shadow-lg py-1 min-w-[190px] overflow-y-auto"
            style={{ left: contextMenu.x, top: contextMenu.y, maxHeight: "calc(100dvh - 16px)" }}
          >
            {project.registeredProjects.map((reg) => (
              <button
                key={`${reg.scope}:${reg.path}`}
                onClick={() => {
                  setContextMenu(null);
                  onEditProject(reg);
                }}
                data-testid="sidebar-project-context-menu-edit"
                className="w-full text-left px-3 py-2 max-md:py-3 text-sm text-text-secondary hover:bg-surface-700/50 cursor-pointer transition-colors"
              >
                {project.registeredProjects.length > 1 ? `Edit base branch (${reg.scope})` : "Edit base branch"}
              </button>
            ))}
            <div className="border-t border-surface-700/20 my-1" />
            <button
              onClick={() => {
                setContextMenu(null);
                onRemoveProject(project);
              }}
              data-testid="sidebar-project-context-menu-remove"
              className="w-full text-left px-3 py-2 max-md:py-3 text-sm text-text-secondary hover:bg-surface-700/50 cursor-pointer transition-colors"
            >
              Remove project
            </button>
          </div>,
          document.body,
        )}
    </>
  );
});
