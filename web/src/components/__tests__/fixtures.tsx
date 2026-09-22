/* eslint-disable react-refresh/only-export-components */
import { useMemo, useRef, type ReactNode } from "react";
import { fireEvent, render, screen } from "@testing-library/react";
import { vi } from "vitest";
import { useSidebarTriage } from "../../hooks/useSidebarTriage";
import type { SessionResponse, Workspace } from "../../lib/types";
import { SessionColorsContext } from "../../lib/sessionColors";
import { SessionRowTagContext, type SessionRowTagMode } from "../../lib/sessionRowTag";
import { UnreadIndicatorContext } from "../../lib/unreadIndicator";
import { DragSuppressContext } from "../sidebar/dnd";
import { SessionRow } from "../sidebar/SessionRow";
import type { RowBulkApi } from "../sidebar/types";

export function makeSession(over: Partial<SessionResponse> = {}): SessionResponse {
  return {
    id: "s1",
    title: "row title",
    project_path: "/p",
    group_path: "/p",
    tool: "claude",
    status: "Idle",
    yolo_mode: false,
    created_at: "2025-01-01T00:00:00Z",
    last_accessed_at: null,
    idle_entered_at: null,
    last_error: null,
    branch: null,
    main_repo_path: null,
    is_sandboxed: false,
    favorited: false,
    has_managed_worktree: false,
    has_terminal: true,
    profile: "default",
    cleanup_defaults: { delete_worktree: false, delete_branch: false, delete_sandbox: false, delete_to_trash: false },
    remote_owner: null,
    notify_on_waiting: null,
    notify_on_idle: null,
    notify_on_error: null,
    claude_fullscreen: false,
    workspace_repos: [],
    ...over,
  } as SessionResponse;
}

export function makeWorkspace(id: string, sessions: SessionResponse[], over: Partial<Workspace> = {}): Workspace {
  return {
    id,
    branch: null,
    projectPath: "/p",
    displayName: id,
    agents: ["claude"],
    primaryAgent: "claude",
    status: "idle",
    sessions,
    ...over,
  } as Workspace;
}

export function jsonResponse(body: unknown, status = 200) {
  return new Response(JSON.stringify(body), { status, headers: { "content-type": "application/json" } });
}

/** Stubs `fetch` with a spy answering `{ id: "s1" }`; call from `beforeEach` and unstub globals after. */
export function stubFetch() {
  const spy = vi.fn<typeof fetch>(async () => jsonResponse({ id: "s1" }));
  vi.stubGlobal("fetch", spy);
  return spy;
}

/** The JSON body of the spy's first call, plus its url and method. */
export function firstRequest(spy: ReturnType<typeof stubFetch>) {
  const [url, init] = spy.mock.calls[0]!;
  return { url, method: init?.method, body: JSON.parse(init!.body as string) };
}

const SINGLE_BULK_API: RowBulkApi = {
  prepareScope: () => ({ kind: "single" }),
  pin: () => {},
  archive: () => {},
  snooze: () => {},
};

interface RowOptions {
  readOnly?: boolean;
  isActive?: boolean;
  onCreateSession?: (repoPath: string) => void;
  rowTagMode?: SessionRowTagMode;
  colorsEnabled?: boolean;
  unread?: boolean;
}

/** A single unselected SessionRow wired to the real triage hook, as the sidebar wires it. */
function Row({ ws, readOnly, isActive = false, onCreateSession }: RowOptions & { ws: Workspace }) {
  const triage = useSidebarTriage(useMemo(() => [ws], [ws]));
  return (
    <SessionRow
      workspace={ws}
      isActive={isActive}
      isSelected={false}
      onActivate={() => {}}
      onCreateSession={onCreateSession}
      readOnly={readOnly}
      optimistic={triage.optimisticFor(ws.id)}
      onPinToggle={triage.pinToggle}
      onArchiveToggle={triage.archiveToggle}
      onSnooze={triage.snooze}
      onUnreadToggle={triage.unreadToggle}
      bulkApi={SINGLE_BULK_API}
    />
  );
}

function Providers({
  children,
  rowTagMode = "branch",
  colorsEnabled = true,
  unread,
}: RowOptions & { children: ReactNode }) {
  const ref = useRef(0);
  const inner = (
    <SessionRowTagContext.Provider value={rowTagMode}>
      <SessionColorsContext.Provider value={colorsEnabled}>{children}</SessionColorsContext.Provider>
    </SessionRowTagContext.Provider>
  );
  return (
    <DragSuppressContext.Provider value={ref}>
      {unread === undefined ? (
        inner
      ) : (
        <UnreadIndicatorContext.Provider value={unread}>{inner}</UnreadIndicatorContext.Provider>
      )}
    </DragSuppressContext.Provider>
  );
}

export function renderRow(ws: Workspace, options: RowOptions = {}) {
  const ui = (w: Workspace, o: RowOptions) => (
    <Providers {...o}>
      <Row ws={w} {...o} />
    </Providers>
  );
  const utils = render(ui(ws, options));
  return { ...utils, rerenderRow: (w: Workspace, o: RowOptions = options) => utils.rerender(ui(w, o)) };
}

/** Renders a row and opens its context menu; returns the menu element. */
export function openRowMenu(ws: Workspace, options: RowOptions = {}) {
  renderRow(ws, options);
  fireEvent.contextMenu(screen.getByTestId("sidebar-session-row"));
  return screen.getByTestId("sidebar-context-menu");
}
