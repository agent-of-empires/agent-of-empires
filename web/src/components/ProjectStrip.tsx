import {
  useEffect,
  useMemo,
  useRef,
  useState,
  type CSSProperties,
  type ReactNode,
} from "react";
import { createPortal } from "react-dom";
import { Activity, CheckCircle2, Plus } from "lucide-react";
import {
  DndContext,
  MouseSensor,
  TouchSensor,
  closestCenter,
  useSensor,
  useSensors,
  type DragEndEvent,
} from "@dnd-kit/core";
import {
  SortableContext,
  arrayMove,
  horizontalListSortingStrategy,
  useSortable,
} from "@dnd-kit/sortable";
import { CSS } from "@dnd-kit/utilities";
import type {
  RepoGroup,
  SessionResponse,
  SessionStatus,
  Workspace,
} from "../lib/types";
import {
  REPO_COLOR_OPTIONS,
  type RepoAppearanceUpdate,
  type RepoColor,
} from "../lib/repoAppearance";
import { getStatusTextClass, isSessionActive } from "../lib/session";
import { useIdleDecayWindowMs } from "../lib/idleDecay";
import { StatusGlyph } from "./StatusGlyph";

interface Props {
  groups: RepoGroup[];
  activeSessionId: string | null;
  activeWorkspaceId: string | null;
  onSelectWorkspace: (workspaceId: string) => void;
  onSelectSession: (sessionId: string) => void;
  onCreateSession: (repoPath: string) => void;
  onDeleteSession?: (workspaceId: string) => void;
  onReorderWorkspaces: (newOrder: string[]) => void;
  onUpdateAppearance: (repoId: string, update: RepoAppearanceUpdate) => void;
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

const RECENT_FINISH_WINDOW_MS = 5 * 60 * 1000;

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

function uniqueGroupSessions(group: RepoGroup): SessionResponse[] {
  const seen = new Set<string>();
  const sessions: SessionResponse[] = [];
  for (const workspace of group.workspaces) {
    for (const session of workspace.sessions) {
      if (seen.has(session.id)) continue;
      seen.add(session.id);
      sessions.push(session);
    }
  }
  return sessions;
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

function repoSwatchStyle(color: RepoColor): CSSProperties {
  return { backgroundColor: `var(${REPO_COLOR_TOKENS[color]})` };
}

function workspaceLabel(workspace: Workspace) {
  return workspace.branch ?? workspace.displayName ?? "default";
}

function hasRecentlyFinishedSession(
  sessions: SessionResponse[],
  idleDecayWindowMs: number,
): boolean {
  const now = Date.now();
  const windowMs = Math.max(idleDecayWindowMs, RECENT_FINISH_WINDOW_MS);
  return sessions.some((session) => {
    if (session.status !== "Idle" || !session.idle_entered_at) return false;
    const idleAt = Date.parse(session.idle_entered_at);
    if (!Number.isFinite(idleAt)) return false;
    const age = now - idleAt;
    return age >= 0 && age <= windowMs;
  });
}

function hasRunningSession(
  sessions: SessionResponse[],
  idleDecayWindowMs: number,
): boolean {
  return sessions.some((session) => isSessionActive(session, idleDecayWindowMs));
}

function SortableProjectChip({
  id,
  readOnly,
  children,
}: {
  id: string;
  readOnly: boolean;
  children: ReactNode;
}) {
  const { listeners, setNodeRef, transform, transition, isDragging } =
    useSortable({ id, disabled: readOnly });
  return (
    <div
      ref={setNodeRef}
      style={{
        transform: CSS.Transform.toString(transform),
        transition,
        zIndex: isDragging ? 20 : "auto",
      }}
      {...listeners}
      className={isDragging ? "relative opacity-80" : "relative"}
    >
      {children}
    </div>
  );
}

export function ProjectStrip({
  groups,
  activeSessionId,
  activeWorkspaceId,
  onSelectWorkspace,
  onSelectSession,
  onCreateSession,
  onDeleteSession,
  onReorderWorkspaces,
  onUpdateAppearance,
  readOnly = false,
}: Props) {
  const idleDecayWindowMs = useIdleDecayWindowMs();
  const activeButtonRef = useRef<HTMLButtonElement | null>(null);
  const menuRef = useRef<HTMLDivElement | null>(null);
  const [menu, setMenu] = useState<{
    groupId: string;
    x: number;
    y: number;
  } | null>(null);
  const [renamingGroupId, setRenamingGroupId] = useState<string | null>(null);
  const [renameValue, setRenameValue] = useState("");
  const renameRef = useRef<HTMLInputElement | null>(null);
  const sensors = useSensors(
    useSensor(MouseSensor, { activationConstraint: { distance: 8 } }),
    useSensor(TouchSensor, { activationConstraint: { delay: 150, tolerance: 8 } }),
  );

  const openMenuForGroup = (groupId: string, element: HTMLElement) => {
    const rect = element.getBoundingClientRect();
    setMenu({ groupId, x: rect.left, y: rect.bottom + 4 });
  };

  const items = useMemo(
    () =>
      groups
        .filter((group) => group.workspaces.length > 0)
        .map((group) => {
          const sessions = groupSessions(group);
          const session = bestSession(sessions, idleDecayWindowMs);
          const active = group.workspaces.some((w) => w.id === activeWorkspaceId);
          return {
            group,
            session,
            active,
            workspaceId: group.workspaces[0]!.id,
            hasRunning: hasRunningSession(sessions, idleDecayWindowMs),
            recentlyFinished: hasRecentlyFinishedSession(
              sessions,
              idleDecayWindowMs,
            ),
          };
        }),
    [groups, activeWorkspaceId, idleDecayWindowMs],
  );

  const activeItem =
    items.find((item) => item.group.workspaces.some((w) => w.id === activeWorkspaceId)) ??
    items[0] ??
    null;

  const activeWorkspaceItems = activeItem?.group.workspaces ?? [];
  const activeSessions = activeItem ? uniqueGroupSessions(activeItem.group) : [];

  const handleProjectDragEnd = (event: DragEndEvent) => {
    const { active, over } = event;
    if (!over || active.id === over.id) return;
    const oldIndex = items.findIndex((item) => item.group.id === active.id);
    const newIndex = items.findIndex((item) => item.group.id === over.id);
    if (oldIndex < 0 || newIndex < 0) return;
    const reordered = arrayMove(items, oldIndex, newIndex);
    onReorderWorkspaces(
      reordered.flatMap((item) => item.group.workspaces.map((w) => w.id)),
    );
  };

  useEffect(() => {
    if (!activeButtonRef.current?.scrollIntoView) return;
    activeButtonRef.current.scrollIntoView({
      block: "nearest",
      inline: "center",
    });
  }, [activeWorkspaceId]);

  useEffect(() => {
    if (renamingGroupId) renameRef.current?.select();
  }, [renamingGroupId]);

  useEffect(() => {
    if (!menu) return;
    const close = (event: MouseEvent) => {
      if (menuRef.current?.contains(event.target as Node)) return;
      setMenu(null);
    };
    const onKeyDown = (event: KeyboardEvent) => {
      if (event.key === "Escape") setMenu(null);
    };
    const id = requestAnimationFrame(() => {
      document.addEventListener("click", close);
      document.addEventListener("contextmenu", close);
      document.addEventListener("keydown", onKeyDown);
    });
    return () => {
      cancelAnimationFrame(id);
      document.removeEventListener("click", close);
      document.removeEventListener("contextmenu", close);
      document.removeEventListener("keydown", onKeyDown);
    };
  }, [menu]);

  const startRename = (group: RepoGroup) => {
    setMenu(null);
    setRenameValue(group.alias ?? group.defaultDisplayName);
    setRenamingGroupId(group.id);
  };

  const commitRename = (group: RepoGroup) => {
    setRenamingGroupId(null);
    const trimmed = renameValue.trim();
    onUpdateAppearance(group.id, { alias: trimmed || null });
  };

  if (groups.length === 0) return null;

  return (
    <nav
      aria-label="Project switcher"
      data-testid="project-strip"
      data-project-strip="true"
      className="h-16 shrink-0 border-b border-surface-700/20 bg-surface-900/95"
    >
      <DndContext
        sensors={sensors}
        collisionDetection={closestCenter}
        onDragEnd={readOnly ? undefined : handleProjectDragEnd}
      >
        <SortableContext
          items={items.map((item) => item.group.id)}
          strategy={horizontalListSortingStrategy}
        >
          <div
            aria-label="Projects"
            className="flex h-8 items-center gap-1 overflow-x-auto border-b border-surface-800/80 px-2 [scrollbar-width:none] [&::-webkit-scrollbar]:hidden"
          >
            {items.map(({ group, session, active, workspaceId, hasRunning, recentlyFinished }) => {
              const status = session?.status ?? "Unknown";
              return (
                <SortableProjectChip
                  key={group.id}
                  id={group.id}
                  readOnly={readOnly}
                >
                  <div
                    className={`group flex h-7 min-w-[4.5rem] max-w-[9rem] items-center rounded-md border transition-colors ${
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
                      {renamingGroupId === group.id ? (
                        <input
                          ref={renameRef}
                          type="text"
                          value={renameValue}
                          onChange={(e) => setRenameValue(e.target.value)}
                          onClick={(e) => e.stopPropagation()}
                          onDoubleClick={(e) => e.stopPropagation()}
                          onBlur={() => commitRename(group)}
                          onKeyDown={(e) => {
                            e.stopPropagation();
                            if (e.key === "Enter") commitRename(group);
                            if (e.key === "Escape") setRenamingGroupId(null);
                          }}
                          data-testid="project-strip-rename-input"
                          className="h-6 min-w-0 flex-1 rounded-md border border-brand-600 bg-surface-950 px-1.5 text-center text-[11px] text-text-primary outline-none"
                        />
                      ) : (
                        <>
                          <span className="mr-1 flex shrink-0 items-center gap-0.5">
                            {hasRunning && (
                              <Activity
                                aria-label="Running session in project"
                                className="h-3 w-3 text-status-running"
                              />
                            )}
                            {recentlyFinished && (
                              <CheckCircle2
                                aria-label="Recently finished session in project"
                                className="h-3 w-3 text-status-running"
                              />
                            )}
                          </span>
                          <span className="min-w-0 flex-1 text-center">
                            <span className="block truncate text-[11px] font-medium leading-4">
                              {group.displayName}
                            </span>
                          </span>
                        </>
                      )}
                    </button>
                    {menu?.groupId === group.id &&
                      createPortal(
                        <div
                          ref={menuRef}
                          role="menu"
                          data-testid="project-strip-menu"
                          style={{ left: menu.x, top: menu.y }}
                          className="fixed z-50 w-48 rounded-md border border-surface-700 bg-surface-950 p-1 shadow-lg"
                        >
                          <button
                            type="button"
                            role="menuitem"
                            onClick={() => startRename(group)}
                            className="h-8 w-full rounded-md px-2 text-left text-[12px] text-text-secondary transition-colors hover:bg-surface-800"
                          >
                            Rename project
                          </button>
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
                          {!readOnly && onDeleteSession && (
                            <button
                              type="button"
                              role="menuitem"
                              onClick={() => {
                                setMenu(null);
                                onDeleteSession(workspaceId);
                              }}
                              className="h-8 w-full rounded-md px-2 text-left text-[12px] text-status-error transition-colors hover:bg-status-error/10"
                            >
                              Delete current session
                            </button>
                          )}
                          <div className="border-t border-surface-700/20 my-1" />
                          <div className="px-2 py-1 text-[11px] font-mono uppercase tracking-widest text-text-muted">
                            Background
                          </div>
                          <div className="grid grid-cols-4 gap-1 px-2 py-1">
                            {REPO_COLOR_OPTIONS.map((option) => (
                              <button
                                key={option.id}
                                type="button"
                                onClick={() => {
                                  setMenu(null);
                                  onUpdateAppearance(group.id, { color: option.id });
                                }}
                                data-testid={`project-strip-color-${option.id}`}
                                aria-label={`Set ${option.label} background`}
                                className={`h-8 rounded-md border cursor-pointer transition-colors ${
                                  group.color === option.id
                                    ? "border-text-primary"
                                    : "border-surface-700"
                                }`}
                                style={repoSwatchStyle(option.id)}
                              />
                            ))}
                            <button
                              type="button"
                              onClick={() => {
                                setMenu(null);
                                onUpdateAppearance(group.id, { color: null });
                              }}
                              data-testid="project-strip-color-clear"
                              aria-label="Clear background"
                              className="h-8 rounded-md border border-surface-700 bg-surface-900 text-[10px] font-mono text-text-dim cursor-pointer hover:bg-surface-700/40"
                            >
                              None
                            </button>
                          </div>
                        </div>,
                        document.body,
                      )}
                  </div>
                </SortableProjectChip>
              );
            })}
          </div>
        </SortableContext>
      </DndContext>
      <div
        aria-label="Sessions in selected project"
        className="flex h-8 items-center gap-1 overflow-x-auto border-t border-surface-800/80 px-2 [scrollbar-width:none] [&::-webkit-scrollbar]:hidden"
      >
        {activeSessions.map((session) => {
          const textClass = getStatusTextClass(
            {
              status: session.status,
              idle_entered_at: session.idle_entered_at,
            },
            idleDecayWindowMs,
          );
          const title = session.title.trim() || session.tool;
          const workspace = activeWorkspaceItems.find((w) =>
            w.sessions.some((s) => s.id === session.id),
          );
          const label = workspace ? workspaceLabel(workspace) : null;
          return (
            <button
              key={session.id}
              type="button"
              aria-current={session.id === activeSessionId ? "page" : undefined}
              data-testid="project-strip-session"
              onClick={() => onSelectSession(session.id)}
              className={`flex h-7 w-[8.5rem] shrink-0 items-center gap-1 rounded-md px-1.5 text-left transition-colors ${
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
                {label && (
                  <span className="block truncate text-[9px] leading-3 text-text-dim">
                    {label}
                  </span>
                )}
              </span>
            </button>
          );
        })}
      </div>
    </nav>
  );
}
