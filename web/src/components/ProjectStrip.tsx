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
import { StatusGlyph } from "./StatusGlyph";
import { OwnerAvatar } from "./OwnerAvatar";

interface Props {
  groups: RepoGroup[];
  activeSessionId: string | null;
  activeWorkspaceId: string | null;
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

function bestSession(
  sessions: SessionResponse[],
  idleDecayWindowMs: number,
): SessionResponse | null {
  const active = sessions.find((s) => isSessionActive(s, idleDecayWindowMs));
  if (active) return active;

  return (
    [...sessions].sort(
      (a, b) =>
        STATUS_PRIORITY.indexOf(a.status) - STATUS_PRIORITY.indexOf(b.status),
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

function matchesFilter(group: RepoGroup, query: string) {
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

function workspaceLabel(workspace: Workspace) {
  return workspace.branch ?? workspace.displayName ?? "default";
}

export function ProjectStrip({
  groups,
  activeSessionId,
  activeWorkspaceId,
  onSelectWorkspace,
  onSelectSession,
  onCreateSession,
  readOnly = false,
}: Props) {
  const idleDecayWindowMs = useIdleDecayWindowMs();
  const [filter, setFilter] = useState("");
  const activeButtonRef = useRef<HTMLButtonElement | null>(null);

  const items = useMemo(
    () =>
      groups
        .filter((group) => group.workspaces.length > 0)
        .filter((group) => matchesFilter(group, filter.trim()))
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

  const totalSessions = groups.reduce(
    (sum, group) => sum + groupSessions(group).length,
    0,
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
      className="h-[128px] shrink-0 border-b border-surface-700/20 bg-surface-900/95"
    >
      <div className="flex h-10 items-center gap-2 border-b border-surface-800/80 px-2">
        <div className="min-w-0 shrink-0">
          <div className="font-mono text-[10px] uppercase tracking-widest text-text-muted">
            Projects
          </div>
          <div className="font-mono text-[10px] text-text-dim">
            {items.length}/{groups.length} · {totalSessions} sessions
          </div>
        </div>
        <label className="relative min-w-[14rem] flex-1">
          <Search
            aria-hidden="true"
            className="pointer-events-none absolute left-2 top-1/2 h-3.5 w-3.5 -translate-y-1/2 text-text-dim"
          />
          <input
            aria-label="Filter project strip"
            data-testid="project-strip-filter"
            type="search"
            value={filter}
            onChange={(e) => setFilter(e.target.value)}
            placeholder="Filter projects, branches, agents..."
            className="h-8 w-full rounded-md border border-surface-700 bg-surface-950 pl-7 pr-2 font-mono text-[12px] text-text-primary outline-none transition-colors placeholder:text-text-dim focus:border-brand-600"
          />
        </label>
      </div>
      <div
        role="tablist"
        aria-label="Projects"
        className="flex h-11 items-center gap-1 overflow-x-auto px-2 [scrollbar-width:thin]"
      >
        {items.map(({ group, session, active, workspaceId, count }) => {
          const status = session?.status ?? "Unknown";
          const textClass = getStatusTextClass(
            {
              status,
              idle_entered_at: session?.idle_entered_at ?? null,
            },
            idleDecayWindowMs,
          );
          return (
            <div
              key={group.id}
              className={`group flex h-9 min-w-[15rem] max-w-[22rem] items-center rounded-md border transition-colors ${
                active
                  ? "border-brand-600 bg-surface-800 text-text-primary"
                  : "border-transparent text-text-muted hover:border-surface-700 hover:bg-surface-800/70 hover:text-text-secondary"
              }`}
              style={repoColorStyle(group.color)}
            >
              <button
                ref={active ? activeButtonRef : undefined}
                type="button"
                role="tab"
                aria-selected={active}
                data-testid="project-strip-tab"
                onClick={() => onSelectWorkspace(workspaceId)}
                className="flex h-full min-w-0 flex-1 items-center gap-2 px-2 text-left"
                title={group.repoPath}
              >
                <span
                  className={`w-4 shrink-0 text-center font-mono text-[12px] ${textClass}`}
                  aria-hidden="true"
                >
                  <StatusGlyph
                    status={status}
                    createdAt={session?.created_at ?? null}
                    idleEnteredAt={session?.idle_entered_at ?? null}
                  />
                </span>
                <OwnerAvatar owner={group.remoteOwner} size={16} />
                <span className="min-w-0 flex-1">
                  <span className="block truncate text-[12px] font-medium leading-4">
                    {group.displayName}
                  </span>
                  <span className="block truncate font-mono text-[10px] leading-3 text-text-dim">
                    {session?.tool ?? "agent"} · {count} session
                    {count === 1 ? "" : "s"}
                  </span>
                </span>
                <span className="shrink-0 rounded border border-surface-700/70 px-1 font-mono text-[10px] text-text-dim">
                  {group.workspaces.length}
                </span>
              </button>
              <button
                type="button"
                aria-label={`New session in ${group.displayName}`}
                disabled={readOnly}
                onClick={() => {
                  onCreateSession(group.repoPath);
                }}
                className="mr-1 flex h-7 w-7 shrink-0 items-center justify-center rounded-md text-text-muted opacity-70 transition-colors hover:bg-surface-700/60 hover:text-text-secondary group-hover:opacity-100 disabled:cursor-not-allowed disabled:opacity-30"
              >
                <Plus className="h-3.5 w-3.5" />
              </button>
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
        className="flex h-[76px] items-start gap-1 overflow-x-auto border-t border-surface-800/80 px-2 py-1.5 [scrollbar-width:thin]"
      >
        {activeWorkspaceItems.map((workspace) => {
          const label = workspaceLabel(workspace);
          return (
            <div
              key={workspace.id}
              className="flex h-16 shrink-0 items-center gap-1 rounded-md border border-surface-800 bg-surface-950/40 px-1.5"
            >
              <button
                type="button"
                onClick={() => onSelectWorkspace(workspace.id)}
                className={`h-12 w-[9.5rem] rounded px-1.5 text-left transition-colors ${
                  workspace.id === activeWorkspaceId
                    ? "bg-surface-800 text-text-primary"
                    : "text-text-muted hover:bg-surface-800/70 hover:text-text-secondary"
                }`}
                title={workspace.projectPath}
              >
                <span className="block truncate text-[11px] font-medium leading-4">
                  {label}
                </span>
                <span className="block truncate font-mono text-[9px] leading-3 text-text-dim">
                  {workspace.primaryAgent} · {workspace.sessions.length} session
                  {workspace.sessions.length === 1 ? "" : "s"}
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
                    className={`flex h-12 w-[13.5rem] items-center gap-1.5 rounded px-1.5 text-left transition-colors ${
                      session.id === activeSessionId
                        ? "bg-surface-800 text-text-primary"
                        : "text-text-muted hover:bg-surface-800/70 hover:text-text-secondary"
                    }`}
                    title={`${title} · ${session.project_path}`}
                  >
                    <span
                      className={`w-3 shrink-0 text-center font-mono text-[10px] ${textClass}`}
                      aria-hidden="true"
                    >
                      <StatusGlyph
                        status={session.status}
                        createdAt={session.created_at}
                        idleEnteredAt={session.idle_entered_at}
                      />
                    </span>
                    <span className="min-w-0">
                      <span className="block truncate text-[11px] leading-4">
                        {title}
                      </span>
                      <span className="block truncate font-mono text-[9px] leading-3 text-text-dim">
                        {session.tool} · {session.status}
                        {session.branch ? ` · ${session.branch}` : ""}
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
