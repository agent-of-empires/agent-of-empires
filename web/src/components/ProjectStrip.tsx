import { useEffect, useMemo, useRef, useState, type CSSProperties } from "react";
import { Plus, Search } from "lucide-react";
import type {
  RepoGroup,
  SessionResponse,
  SessionStatus,
  Workspace,
} from "../lib/types";
import type { RepoColor } from "../lib/repoAppearance";
import { getStatusTextClass, isSessionActive } from "../lib/session";
import { useIdleDecayWindowMs } from "../lib/idleDecay";
import { matchesProjectStripFilter } from "../lib/projectStrip";
import { StatusGlyph } from "./StatusGlyph";

interface Props {
  groups: RepoGroup[];
  activeSessionId: string | null;
  activeWorkspaceId: string | null;
  filter: string;
  onFilterChange: (filter: string) => void;
  onSelectWorkspace: (workspaceId: string) => void;
  onSelectSession: (sessionId: string) => void;
  onCreateSession: (repoPath: string) => void;
  readOnly?: boolean;
}

const STATUS_PRIORITY: SessionStatus[] = [
  "Running",
  "Waiting",
  "Starting",
  "Creating",
  "Error",
  "Idle",
  "Stopped",
  "Unknown",
  "Deleting",
];

function statusRank(status: SessionStatus): number {
  const idx = STATUS_PRIORITY.indexOf(status);
  return idx === -1 ? STATUS_PRIORITY.length : idx;
}

function bestSession(
  sessions: SessionResponse[],
  idleDecayWindowMs: number,
): SessionResponse | null {
  const active = sessions.find((s) => isSessionActive(s, idleDecayWindowMs));
  if (active) return active;

  return (
    [...sessions].sort(
      (a, b) => statusRank(a.status) - statusRank(b.status),
    )[0] ?? null
  );
}

function groupSessions(group: RepoGroup): SessionResponse[] {
  return group.workspaces.flatMap((workspace) => workspace.sessions);
}

const REPO_COLOR_TOKENS: Record<RepoColor, string> = {
  amber: "--color-status-waiting",
  teal: "--color-terminal-active",
  sky: "--color-sandbox",
  violet: "--color-diff-header",
  rose: "--color-status-error",
  slate: "--color-surface-700",
};

function repoColorStyle(color: RepoColor | null): CSSProperties | undefined {
  if (!color) return undefined;
  const token = REPO_COLOR_TOKENS[color];
  return {
    backgroundColor: `color-mix(in srgb, var(${token}) 14%, transparent)`,
    borderColor: `color-mix(in srgb, var(${token}) 42%, var(--color-surface-700))`,
  };
}

function workspaceLabel(workspace: Workspace) {
  return workspace.branch ?? workspace.displayName ?? "default";
}

export function ProjectStrip({
  groups,
  activeSessionId,
  activeWorkspaceId,
  filter,
  onFilterChange,
  onSelectWorkspace,
  onSelectSession,
  onCreateSession,
  readOnly = false,
}: Props) {
  const idleDecayWindowMs = useIdleDecayWindowMs();
  const activeButtonRef = useRef<HTMLButtonElement | null>(null);
  const [menu, setMenu] = useState<{
    groupId: string;
    x: number;
    y: number;
  } | null>(null);

  const openMenuForGroup = (groupId: string, element: HTMLElement) => {
    const rect = element.getBoundingClientRect();
    setMenu({ groupId, x: rect.left, y: rect.bottom + 4 });
  };

  const items = useMemo(
    () =>
      groups
        .filter((group) => group.workspaces.length > 0)
        .filter((group) => matchesProjectStripFilter(group, filter.trim()))
        .map((group) => {
          const sessions = groupSessions(group);
          const session = bestSession(sessions, idleDecayWindowMs);
          const active = group.workspaces.some((w) => w.id === activeWorkspaceId);
          return {
            group,
            session,
            active,
            workspaceId: group.workspaces[0]!.id,
            count: sessions.length,
          };
        }),
    [groups, filter, activeWorkspaceId, idleDecayWindowMs],
  );

  const activeItem =
    items.find((item) => item.group.workspaces.some((w) => w.id === activeWorkspaceId)) ??
    items[0] ??
    null;

  const activeWorkspaceItems = activeItem?.group.workspaces ?? [];

  useEffect(() => {
    if (!activeButtonRef.current?.scrollIntoView) return;
    activeButtonRef.current.scrollIntoView({
      block: "nearest",
      inline: "center",
    });
  }, [activeWorkspaceId]);

  if (groups.length === 0) return null;

  return (
    <nav
      aria-label="Project switcher"
      data-testid="project-strip"
      className="h-[72px] shrink-0 border-b border-surface-700/20 bg-surface-900/95"
    >
      <div className="flex h-8 items-center justify-end border-b border-surface-800/80 px-2">
        <label className="relative w-full max-w-[18rem]">
          <Search
            aria-hidden="true"
            className="pointer-events-none absolute left-2 top-1/2 h-3.5 w-3.5 -translate-y-1/2 text-text-dim"
          />
          <input
            aria-label="Filter project strip"
            data-testid="project-strip-filter"
            type="search"
            value={filter}
            onChange={(e) => onFilterChange(e.target.value)}
            placeholder="Filter projects, branches, agents..."
            className="h-8 w-full rounded-md border border-surface-700 bg-surface-950 pl-7 pr-2 font-sans text-[11px] text-text-primary outline-none transition-colors placeholder:text-text-dim focus:border-brand-600"
          />
        </label>
      </div>
      <div
        aria-label="Projects"
        className="flex h-8 items-center gap-1 overflow-x-auto px-2 [scrollbar-width:none] [&::-webkit-scrollbar]:hidden"
      >
        {items.map(({ group, session, active, workspaceId }) => {
          const status = session?.status ?? "Unknown";
          return (
            <div
              key={group.id}
              className={`group relative flex h-7 min-w-[4.5rem] max-w-[9rem] items-center rounded-md border transition-colors ${
                active
                  ? "border-brand-600 bg-surface-800 text-text-primary"
                  : "border-transparent text-text-muted hover:border-surface-700 hover:bg-surface-800/70 hover:text-text-secondary"
              }`}
              style={repoColorStyle(group.color)}
            >
              <button
                ref={active ? activeButtonRef : undefined}
                type="button"
                aria-selected={active}
                aria-haspopup="menu"
                data-testid="project-strip-tab"
                onClick={() => onSelectWorkspace(workspaceId)}
                onContextMenu={(e) => {
                  e.preventDefault();
                  openMenuForGroup(group.id, e.currentTarget);
                }}
                onDoubleClick={(e) => {
                  e.preventDefault();
                  openMenuForGroup(group.id, e.currentTarget);
                }}
                onKeyDown={(e) => {
                  if (e.target !== e.currentTarget) return;
                  if (
                    e.key !== "Enter" &&
                    e.key !== " " &&
                    e.key !== "ContextMenu" &&
                    !(e.shiftKey && e.key === "F10")
                  ) {
                    return;
                  }
                  e.preventDefault();
                  openMenuForGroup(group.id, e.currentTarget);
                }}
                className="flex h-full min-w-0 flex-1 items-center px-2 text-left"
                title={`${group.displayName} · ${status} · ${group.repoPath}`}
              >
                <span className="min-w-0 flex-1 text-center">
                  <span className="block truncate text-[11px] font-medium leading-4">
                    {group.displayName}
                  </span>
                </span>
              </button>
              {menu?.groupId === group.id && (
                <div
                  role="menu"
                  data-testid="project-strip-menu"
                  style={{ left: menu.x, top: menu.y }}
                  className="fixed z-30 w-44 rounded-md border border-surface-700 bg-surface-950 p-1 shadow-lg"
                >
                  <button
                    type="button"
                    role="menuitem"
                    disabled={readOnly}
                    onClick={() => {
                      setMenu(null);
                      onCreateSession(group.repoPath);
                    }}
                    className="flex h-8 w-full items-center gap-2 rounded-md px-2 text-left text-[12px] text-text-secondary transition-colors hover:bg-surface-800 disabled:cursor-not-allowed disabled:opacity-40"
                  >
                    <Plus className="h-3.5 w-3.5" />
                    New session
                  </button>
                  <button
                    type="button"
                    role="menuitem"
                    onClick={() => setMenu(null)}
                    className="h-8 w-full rounded-md px-2 text-left text-[12px] text-text-muted transition-colors hover:bg-surface-800"
                  >
                    Close
                  </button>
                </div>
              )}
            </div>
          );
        })}
        {items.length === 0 && (
          <div className="flex h-9 items-center px-2 text-[12px] text-text-dim">
            No projects match the filter.
          </div>
        )}
      </div>
      <div
        aria-label="Sessions in selected project"
        className="flex h-8 items-center gap-1 overflow-x-auto border-t border-surface-800/80 px-2 [scrollbar-width:none] [&::-webkit-scrollbar]:hidden"
      >
        {activeWorkspaceItems.map((workspace) => {
          const label = workspaceLabel(workspace);
          return (
            <div
              key={workspace.id}
              className="flex h-7 shrink-0 items-center gap-1 rounded-md border border-surface-800 bg-surface-950/40 px-1"
            >
              <button
                type="button"
                onClick={() => onSelectWorkspace(workspace.id)}
                className={`h-7 w-[6.5rem] rounded-md px-1.5 text-left transition-colors ${
                  workspace.id === activeWorkspaceId
                    ? "bg-surface-800 text-text-primary"
                    : "text-text-muted hover:bg-surface-800/70 hover:text-text-secondary"
                }`}
                title={workspace.projectPath}
              >
                <span className="block truncate text-[10px] font-medium leading-3">
                  {label}
                </span>
              </button>
              {workspace.sessions.map((session) => {
                const textClass = getStatusTextClass(
                  {
                    status: session.status,
                    idle_entered_at: session.idle_entered_at,
                  },
                  idleDecayWindowMs,
                );
                const title = session.title.trim() || session.tool;
                return (
                  <button
                    key={session.id}
                    type="button"
                    aria-current={session.id === activeSessionId ? "page" : undefined}
                    data-testid="project-strip-session"
                    onClick={() => onSelectSession(session.id)}
                    className={`flex h-7 w-[7.5rem] items-center gap-1 rounded-md px-1.5 text-left transition-colors ${
                      session.id === activeSessionId
                        ? "bg-surface-800 text-text-primary"
                        : "text-text-muted hover:bg-surface-800/70 hover:text-text-secondary"
                    }`}
                    title={`${title} · ${session.project_path}`}
                  >
                    <span
                      className={`w-3 shrink-0 text-center font-mono text-[9px] ${textClass}`}
                      aria-hidden="true"
                    >
                      <StatusGlyph
                        status={session.status}
                        createdAt={session.created_at}
                        idleEnteredAt={session.idle_entered_at}
                      />
                    </span>
                    <span className="min-w-0">
                      <span className="block truncate text-[10px] leading-3">
                        {title}
                      </span>
                    </span>
                  </button>
                );
              })}
            </div>
          );
        })}
      </div>
    </nav>
  );
}
