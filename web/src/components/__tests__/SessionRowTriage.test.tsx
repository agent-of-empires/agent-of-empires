// @vitest-environment jsdom
//
// RTL coverage for the new triage affordances on the sidebar
// `SessionRow`: the Pin glyph, the Archive chip, the Snooze chip
// (with the static remaining-time label), and the optimistic flip
// invariants. Each case wires the smallest possible Workspace + a
// DragSuppressContext stub so the row mounts without dragging into
// the dnd-kit plumbing that the production tree provides.

import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { cleanup, fireEvent, render, screen } from "@testing-library/react";
import { useMemo, useRef, type ReactNode } from "react";

import { DragSuppressContext, SessionRow, type RowBulkApi } from "../WorkspaceSidebar";

// Single-row stub for the bulk-triage bridge: these tests mount one
// unselected row, so the context menu is always single-scope. See #2312.
const SINGLE_BULK_API: RowBulkApi = {
  prepareScope: () => ({ kind: "single" }),
  pin: () => {},
  archive: () => {},
  snooze: () => {},
};
import { UnreadIndicatorContext } from "../../lib/unreadIndicator";
import { SessionRowTagContext, type SessionRowTagMode } from "../../lib/sessionRowTag";
import { useSidebarTriage } from "../../hooks/useSidebarTriage";
import type { SessionResponse, Workspace } from "../../lib/types";
import { OPEN_SESSION_EVENT } from "../../lib/sessionRoute";
import { OPEN_SWITCH_AGENT_EVENT, consumePendingSwitchAgent } from "../../lib/switchAgentTrigger";

function session(over: Partial<SessionResponse> = {}): SessionResponse {
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
    cleanup_defaults: {
      delete_worktree: false,
      delete_branch: false,
      delete_sandbox: false,
    },
    remote_owner: null,
    notify_on_waiting: null,
    notify_on_idle: null,
    notify_on_error: null,
    claude_fullscreen: false,
    workspace_repos: [],
    ...over,
  };
}

function workspace(id: string, sessions: SessionResponse[]): Workspace {
  return {
    id,
    branch: null,
    projectPath: "/p",
    displayName: id,
    agents: ["claude"],
    primaryAgent: "claude",
    status: "idle",
    sessions,
  };
}

function Wrap({ children, rowTagMode = "branch" }: { children: ReactNode; rowTagMode?: SessionRowTagMode }) {
  const ref = useRef(0);
  return (
    <DragSuppressContext.Provider value={ref}>
      <SessionRowTagContext.Provider value={rowTagMode}>{children}</SessionRowTagContext.Provider>
    </DragSuppressContext.Provider>
  );
}

// Mounts a SessionRow wired to the real `useSidebarTriage` controller, the
// same way `WorkspaceSidebar` wires it in production. Triage state and the
// pin/archive/snooze PATCH calls live in the hook now (lifted out of the row
// so bulk actions can share them, see #1724), so the row + hook are
// exercised together here rather than the row owning the mutation. Returns
// `null` while the workspace has no row to render.
function Row({
  ws,
  readOnly,
  onCreateSession,
  isActive = false,
}: {
  ws: Workspace;
  readOnly?: boolean;
  onCreateSession?: (repoPath: string) => void;
  isActive?: boolean;
}) {
  const workspaces = useMemo(() => [ws], [ws]);
  const triage = useSidebarTriage(workspaces);
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

const fetchSpy = vi.fn<typeof fetch>();

beforeEach(() => {
  fetchSpy.mockReset();
  vi.stubGlobal("fetch", fetchSpy);
  fetchSpy.mockImplementation(
    async () =>
      new Response(JSON.stringify({ id: "s1" }), {
        status: 200,
        headers: { "content-type": "application/json" },
      }),
  );
});

afterEach(() => {
  cleanup();
  vi.unstubAllGlobals();
  // Drain any switch-agent latch a click left behind so tests stay
  // independent.
  consumePendingSwitchAgent("sess-switch-it");
});

describe("SessionRow chips", () => {
  it("renders the Pin glyph when any session is pinned", () => {
    const ws = workspace("w-pinned", [session({ pinned_at: "2026-01-01T00:00:00Z" })]);
    render(
      <Wrap>
        <Row ws={ws} />
      </Wrap>,
    );
    expect(screen.queryByLabelText("Pinned")).not.toBeNull();
    expect(screen.queryByLabelText("Archived")).toBeNull();
    expect(screen.queryByLabelText("Snoozed")).toBeNull();
  });

  it("renders the monitoring badge when the first session has an armed monitor", () => {
    // A monitor-parked session would otherwise look like a plain idle dot;
    // the badge signals it is waiting on a background watch, not dead.
    const ws = workspace("w-monitor", [session({ monitor_active: true, monitor_description: "clippy passes" })]);
    render(
      <Wrap>
        <Row ws={ws} />
      </Wrap>,
    );
    const badge = screen.getByLabelText("Monitoring clippy passes");
    expect(badge.textContent).toContain("monitoring");
  });

  it("renders no monitoring badge when no monitor is armed", () => {
    const ws = workspace("w-none", [session()]);
    render(
      <Wrap>
        <Row ws={ws} />
      </Wrap>,
    );
    expect(screen.queryByLabelText(/^Monitoring/)).toBeNull();
  });

  it("renders the Archived chip when any session is archived", () => {
    const ws = workspace("w-archived", [session({ archived_at: "2026-01-01T00:00:00Z" })]);
    render(
      <Wrap>
        <Row ws={ws} />
      </Wrap>,
    );
    expect(screen.queryByLabelText("Archived")).not.toBeNull();
    expect(screen.queryByLabelText("Pinned")).toBeNull();
    expect(screen.queryByLabelText("Snoozed")).toBeNull();
  });

  it("renders the Snoozed chip with a remaining-time label", () => {
    const future = new Date(Date.now() + 90 * 60 * 1000).toISOString();
    const ws = workspace("w-snoozed", [session({ snoozed_until: future })]);
    render(
      <Wrap>
        <Row ws={ws} />
      </Wrap>,
    );
    const chip = screen.queryByLabelText("Snoozed");
    expect(chip).not.toBeNull();
    // Bucket sizes: < 1h → minutes, ≥ 1h → "Nh". 90 minutes falls
    // into the 1h bucket. Allow ±1 due to rounding.
    expect(chip!.textContent).toMatch(/1h/);
    expect(screen.queryByLabelText("Archived")).toBeNull();
  });

  it("hides the Snoozed chip when archived (archive wins visually)", () => {
    const ws = workspace("w-both", [
      session({
        archived_at: "2026-01-01T00:00:00Z",
        snoozed_until: "2099-01-01T00:00:00Z",
      }),
    ]);
    render(
      <Wrap>
        <Row ws={ws} />
      </Wrap>,
    );
    expect(screen.queryByLabelText("Archived")).not.toBeNull();
    // Visual gate: chip only renders for !effectiveArchived &&
    // effectiveSnoozed. The data layer prevents both flags from
    // coexisting at the session level, but defensive rendering
    // hides the snooze chip if the workspace surfaces both.
    expect(screen.queryByLabelText("Snoozed")).toBeNull();
  });
});

describe("SessionRow row tags", () => {
  it("renders a compact branch tag and removes the hardcoded branch subtitle", () => {
    const ws = {
      ...workspace("w-branch", [session({ branch: "feature/web-row-tag" })]),
      branch: "feature/web-row-tag",
    };
    render(
      <Wrap rowTagMode="branch">
        <Row ws={ws} />
      </Wrap>,
    );

    expect(screen.getByTestId("sidebar-session-row-tag").textContent).toBe("[web-row-tag]");
    expect(screen.queryByText("feature/web-row-tag")).toBeNull();
  });

  it("hides all session suffix metadata when row_tag is none", () => {
    const ws = {
      ...workspace("w-none", [session({ branch: "feature/hidden" })]),
      branch: "feature/hidden",
    };
    render(
      <Wrap rowTagMode="none">
        <Row ws={ws} />
      </Wrap>,
    );

    expect(screen.queryByTestId("sidebar-session-row-tag")).toBeNull();
    expect(screen.queryByText("feature/hidden")).toBeNull();
  });

  it("renders profile, auto, and sandbox tags from the first session", () => {
    const ws = workspace("w-profile", [session({ profile: "forit-backup", is_sandboxed: true })]);

    const { rerender } = render(
      <Wrap rowTagMode="profile">
        <Row ws={ws} />
      </Wrap>,
    );
    expect(screen.getByTestId("sidebar-session-row-tag").textContent).toBe("[fb]");

    rerender(
      <Wrap rowTagMode="auto">
        <Row ws={ws} />
      </Wrap>,
    );
    expect(screen.getByTestId("sidebar-session-row-tag").textContent).toBe("[fb]");

    rerender(
      <Wrap rowTagMode="sandbox">
        <Row ws={ws} />
      </Wrap>,
    );
    expect(screen.getByTestId("sidebar-session-row-tag").textContent).toBe("[sb]");
  });

  it("renders multi-repo workspace branch tags without removing repo chips", () => {
    const ws = workspace("w-workspace", [
      session({
        workspace_repos: [
          { name: "api", source_path: "/repo/api", branch: "feature/web-tags" },
          { name: "web", source_path: "/repo/web", branch: "feature/web-tags" },
        ],
      }),
    ]);

    render(
      <Wrap rowTagMode="branch">
        <Row ws={ws} />
      </Wrap>,
    );

    expect(screen.getByTestId("sidebar-session-row-tag").textContent).toBe("[web-tags+2]");
    expect(screen.getByText("api")).not.toBeNull();
    expect(screen.getByText("web")).not.toBeNull();
  });
});

describe("SessionRow unread dot", () => {
  // Positive control: an idle unread row that is not sunk paints the dot,
  // so the absence assertions below mean suppression, not a broken probe.
  it("renders the unread dot on a live idle unread row", () => {
    const ws = workspace("w-unread", [session({ unread: true })]);
    render(
      <Wrap>
        <UnreadIndicatorContext.Provider value={true}>
          <Row ws={ws} />
        </UnreadIndicatorContext.Provider>
      </Wrap>,
    );
    expect(screen.queryByTestId("sidebar-unread-dot")).not.toBeNull();
  });

  it("suppresses the unread dot when the row is archived (#2571)", () => {
    const ws = workspace("w-unread-archived", [session({ unread: true, archived_at: "2026-01-01T00:00:00Z" })]);
    render(
      <Wrap>
        <UnreadIndicatorContext.Provider value={true}>
          <Row ws={ws} />
        </UnreadIndicatorContext.Provider>
      </Wrap>,
    );
    expect(screen.queryByTestId("sidebar-unread-dot")).toBeNull();
  });

  it("suppresses the unread dot when the row is snoozed (#2571)", () => {
    const future = new Date(Date.now() + 90 * 60 * 1000).toISOString();
    const ws = workspace("w-unread-snoozed", [session({ unread: true, snoozed_until: future })]);
    render(
      <Wrap>
        <UnreadIndicatorContext.Provider value={true}>
          <Row ws={ws} />
        </UnreadIndicatorContext.Provider>
      </Wrap>,
    );
    expect(screen.queryByTestId("sidebar-unread-dot")).toBeNull();
  });
});

describe("SessionRow smart-rename chip", () => {
  it("renders the Auto-name chip when smart_rename is pending", () => {
    const ws = workspace("w-pending", [session({ view: "structured", smart_rename: "pending" })]);
    render(
      <Wrap>
        <Row ws={ws} />
      </Wrap>,
    );
    expect(screen.queryByLabelText("Will auto-name")).not.toBeNull();
    expect(screen.queryByLabelText("Naming")).toBeNull();
  });

  it("renders the Naming chip when smart_rename is running", () => {
    const ws = workspace("w-running", [session({ view: "structured", smart_rename: "running" })]);
    render(
      <Wrap>
        <Row ws={ws} />
      </Wrap>,
    );
    expect(screen.queryByLabelText("Naming")).not.toBeNull();
    expect(screen.queryByLabelText("Will auto-name")).toBeNull();
  });

  it("renders no smart-rename chip when inactive", () => {
    const ws = workspace("w-inactive", [session({ view: "structured", smart_rename: "inactive" })]);
    render(
      <Wrap>
        <Row ws={ws} />
      </Wrap>,
    );
    expect(screen.queryByLabelText("Will auto-name")).toBeNull();
    expect(screen.queryByLabelText("Naming")).toBeNull();
  });
});

describe("SessionRow context menu", () => {
  it("offers Unpin plus Archive and Snooze when pinned", () => {
    const ws = workspace("w-pinned", [session({ pinned_at: "2026-01-01T00:00:00Z" })]);
    render(
      <Wrap>
        <Row ws={ws} />
      </Wrap>,
    );
    const row = screen.getByTestId("sidebar-session-row");
    fireEvent.contextMenu(row);
    const menu = screen.getByTestId("sidebar-context-menu");
    // Archiving or snoozing a pinned session clears the pin on the
    // backend, matching the TUI, so the menu must not force unpin-first.
    expect(menu.textContent).toContain("Unpin");
    expect(menu.textContent).toContain("Archive");
    expect(menu.textContent).toContain("Snooze");
  });

  it("shows only the Unarchive toggle when archived", () => {
    const ws = workspace("w-archived", [session({ archived_at: "2026-01-01T00:00:00Z" })]);
    render(
      <Wrap>
        <Row ws={ws} />
      </Wrap>,
    );
    fireEvent.contextMenu(screen.getByTestId("sidebar-session-row"));
    const menu = screen.getByTestId("sidebar-context-menu");
    expect(menu.textContent).toContain("Unarchive");
    expect(menu.textContent).not.toContain("Pin");
    expect(menu.textContent).not.toContain("Snooze");
  });

  it("shows only the Unsnooze toggle when snoozed", () => {
    const future = new Date(Date.now() + 60 * 60 * 1000).toISOString();
    const ws = workspace("w-snoozed", [session({ snoozed_until: future })]);
    render(
      <Wrap>
        <Row ws={ws} />
      </Wrap>,
    );
    fireEvent.contextMenu(screen.getByTestId("sidebar-session-row"));
    const menu = screen.getByTestId("sidebar-context-menu");
    expect(menu.textContent).toContain("Unsnooze");
    expect(menu.textContent).not.toContain("Pin");
    expect(menu.textContent).not.toContain("Archive");
  });

  it("shows Pin / Archive / Snooze… for a live row", () => {
    const ws = workspace("w-live", [session({})]);
    render(
      <Wrap>
        <Row ws={ws} />
      </Wrap>,
    );
    fireEvent.contextMenu(screen.getByTestId("sidebar-session-row"));
    const menu = screen.getByTestId("sidebar-context-menu");
    expect(menu.textContent).toContain("Pin");
    expect(menu.textContent).toContain("Archive");
    expect(menu.textContent).toContain("Snooze…");
  });

  it("shows Switch agent for a structured view row", () => {
    const ws = workspace("w-structured view", [session({ id: "sess-structured view", view: "structured" })]);
    render(
      <Wrap>
        <Row ws={ws} />
      </Wrap>,
    );
    fireEvent.contextMenu(screen.getByTestId("sidebar-session-row"));
    expect(screen.queryByTestId("sidebar-context-menu-switch-agent")).not.toBeNull();
  });

  it("hides Switch agent for a non-structured view (tmux) row", () => {
    const ws = workspace("w-tmux", [session({ view: "terminal" })]);
    render(
      <Wrap>
        <Row ws={ws} />
      </Wrap>,
    );
    fireEvent.contextMenu(screen.getByTestId("sidebar-session-row"));
    expect(screen.queryByTestId("sidebar-context-menu-switch-agent")).toBeNull();
  });

  it("hides the triage section in read-only mode", () => {
    // structured_view is set so the Switch agent gate is also exercised:
    // it must stay hidden in read-only even on a structured view row.
    const ws = workspace("w-live", [session({ view: "structured" })]);
    render(
      <Wrap>
        <Row ws={ws} readOnly />
      </Wrap>,
    );
    fireEvent.contextMenu(screen.getByTestId("sidebar-session-row"));
    const menu = screen.getByTestId("sidebar-context-menu");
    expect(menu.textContent).not.toContain("Pin");
    expect(menu.textContent).not.toContain("Archive");
    expect(menu.textContent).not.toContain("Snooze");
    expect(menu.textContent).not.toContain("Delete");
    expect(screen.queryByTestId("sidebar-context-menu-switch-agent")).toBeNull();
  });
});

describe("SessionRow triage actions", () => {
  it("Pin click fires PATCH /api/sessions/:id/pin with { pinned: true }", async () => {
    const ws = workspace("w-live", [session({ id: "sess-pin-it" })]);
    render(
      <Wrap>
        <Row ws={ws} />
      </Wrap>,
    );
    fireEvent.contextMenu(screen.getByTestId("sidebar-session-row"));
    fireEvent.click(screen.getByTestId("sidebar-context-menu-pin"));
    // Wait for the async handler.
    await vi.waitFor(() => expect(fetchSpy).toHaveBeenCalled());
    const [url, init] = fetchSpy.mock.calls[0]!;
    expect(url).toBe("/api/sessions/sess-pin-it/pin");
    expect(init?.method).toBe("PATCH");
    expect(JSON.parse(init!.body as string)).toEqual({ pinned: true });
  });

  it("Archive click fires PATCH /api/sessions/:id/archive with { archived: true, kill_pane: true }", async () => {
    const ws = workspace("w-live", [session({ id: "sess-arch-it" })]);
    render(
      <Wrap>
        <Row ws={ws} />
      </Wrap>,
    );
    fireEvent.contextMenu(screen.getByTestId("sidebar-session-row"));
    fireEvent.click(screen.getByTestId("sidebar-context-menu-archive"));
    await vi.waitFor(() => expect(fetchSpy).toHaveBeenCalled());
    const [url, init] = fetchSpy.mock.calls[0]!;
    expect(url).toBe("/api/sessions/sess-arch-it/archive");
    expect(JSON.parse(init!.body as string)).toEqual({
      archived: true,
      kill_pane: true,
    });
  });

  it("optimistically shows the Archived chip immediately on click", async () => {
    // Regression: the chip render used `isArchived` (the prop)
    // instead of `effectiveArchived` (the optimistic override). On
    // click the chip didn't appear until the next sessions-poll
    // confirmed the archive, which felt laggy compared to the
    // immediate pin glyph flip. See CodeRabbit review on #1585.
    const ws = workspace("w-live", [session({ id: "sess-opt-archive" })]);
    render(
      <Wrap>
        <Row ws={ws} />
      </Wrap>,
    );
    fireEvent.contextMenu(screen.getByTestId("sidebar-session-row"));
    fireEvent.click(screen.getByTestId("sidebar-context-menu-archive"));
    // The chip should appear synchronously from the optimistic
    // state flip, before the PATCH response would have time to
    // round-trip.
    await vi.waitFor(() => expect(screen.queryByLabelText("Archived")).not.toBeNull());
  });

  it("Snooze… opens the modal (does NOT POST until a preset is picked)", () => {
    const ws = workspace("w-live", [session({ id: "sess-snooze-it" })]);
    render(
      <Wrap>
        <Row ws={ws} />
      </Wrap>,
    );
    fireEvent.contextMenu(screen.getByTestId("sidebar-session-row"));
    fireEvent.click(screen.getByTestId("sidebar-context-menu-snooze"));
    expect(screen.queryByTestId("snooze-modal")).not.toBeNull();
    expect(fetchSpy).not.toHaveBeenCalled();
  });

  it("Unpin click fires PATCH /api/sessions/:id/pin with { pinned: false }", async () => {
    const ws = workspace("w-pinned", [session({ id: "sess-unpin", pinned_at: "2026-01-01T00:00:00Z" })]);
    render(
      <Wrap>
        <Row ws={ws} />
      </Wrap>,
    );
    fireEvent.contextMenu(screen.getByTestId("sidebar-session-row"));
    fireEvent.click(screen.getByTestId("sidebar-context-menu-pin"));
    await vi.waitFor(() => expect(fetchSpy).toHaveBeenCalled());
    const [url, init] = fetchSpy.mock.calls[0]!;
    expect(url).toBe("/api/sessions/sess-unpin/pin");
    expect(JSON.parse(init!.body as string)).toEqual({ pinned: false });
  });

  it("Unarchive click fires PATCH /api/sessions/:id/archive with { archived: false }", async () => {
    const ws = workspace("w-archived", [session({ id: "sess-unarc", archived_at: "2026-01-01T00:00:00Z" })]);
    render(
      <Wrap>
        <Row ws={ws} />
      </Wrap>,
    );
    fireEvent.contextMenu(screen.getByTestId("sidebar-session-row"));
    fireEvent.click(screen.getByTestId("sidebar-context-menu-archive"));
    await vi.waitFor(() => expect(fetchSpy).toHaveBeenCalled());
    const [url, init] = fetchSpy.mock.calls[0]!;
    expect(url).toBe("/api/sessions/sess-unarc/archive");
    expect(JSON.parse(init!.body as string)).toEqual({
      archived: false,
      kill_pane: true,
    });
  });

  it("reverts optimistic pin override on PATCH failure", async () => {
    // Branch coverage: the wake-call-failed path through togglePin.
    fetchSpy.mockImplementation(async () => new Response("nope", { status: 500 }));
    const ws = workspace("w-live", [session({ id: "sess-pin-fail" })]);
    render(
      <Wrap>
        <Row ws={ws} />
      </Wrap>,
    );
    fireEvent.contextMenu(screen.getByTestId("sidebar-session-row"));
    fireEvent.click(screen.getByTestId("sidebar-context-menu-pin"));
    await vi.waitFor(() => expect(fetchSpy).toHaveBeenCalled());
    // The optimistic pin flipped on, then reverted off. The glyph
    // should not be visible after the failure settles.
    await vi.waitFor(() => expect(screen.queryByLabelText("Pinned")).toBeNull());
  });

  it("reverts optimistic archive override on PATCH failure", async () => {
    fetchSpy.mockImplementation(async () => new Response("nope", { status: 500 }));
    const ws = workspace("w-live", [session({ id: "sess-arch-fail" })]);
    render(
      <Wrap>
        <Row ws={ws} />
      </Wrap>,
    );
    fireEvent.contextMenu(screen.getByTestId("sidebar-session-row"));
    fireEvent.click(screen.getByTestId("sidebar-context-menu-archive"));
    await vi.waitFor(() => expect(fetchSpy).toHaveBeenCalled());
    await vi.waitFor(() => expect(screen.queryByLabelText("Archived")).toBeNull());
  });

  it("Unsnooze click fires PATCH /api/sessions/:id/snooze with { minutes: null }", async () => {
    const future = new Date(Date.now() + 60 * 60 * 1000).toISOString();
    const ws = workspace("w-snoozed", [session({ id: "sess-unsnooze-it", snoozed_until: future })]);
    render(
      <Wrap>
        <Row ws={ws} />
      </Wrap>,
    );
    fireEvent.contextMenu(screen.getByTestId("sidebar-session-row"));
    fireEvent.click(screen.getByTestId("sidebar-context-menu-unsnooze"));
    await vi.waitFor(() => expect(fetchSpy).toHaveBeenCalled());
    const [url, init] = fetchSpy.mock.calls[0]!;
    expect(url).toBe("/api/sessions/sess-unsnooze-it/snooze");
    expect(JSON.parse(init!.body as string)).toEqual({ minutes: null });
  });

  it("Switch agent click navigates to the session and requests the dialog", () => {
    const ws = workspace("w-structured view", [session({ id: "sess-switch-it", view: "structured" })]);
    const opened: string[] = [];
    const switched: string[] = [];
    const onOpen = (e: Event) => opened.push((e as CustomEvent).detail.sessionId);
    const onSwitch = (e: Event) => switched.push((e as CustomEvent).detail.sessionId);
    window.addEventListener(OPEN_SESSION_EVENT, onOpen);
    window.addEventListener(OPEN_SWITCH_AGENT_EVENT, onSwitch);
    try {
      render(
        <Wrap>
          <Row ws={ws} />
        </Wrap>,
      );
      fireEvent.contextMenu(screen.getByTestId("sidebar-session-row"));
      fireEvent.click(screen.getByTestId("sidebar-context-menu-switch-agent"));
      expect(opened).toEqual(["sess-switch-it"]);
      expect(switched).toEqual(["sess-switch-it"]);
      // No PATCH: switching is deferred to the dialog in the composer.
      expect(fetchSpy).not.toHaveBeenCalled();
    } finally {
      window.removeEventListener(OPEN_SESSION_EVENT, onOpen);
      window.removeEventListener(OPEN_SWITCH_AGENT_EVENT, onSwitch);
    }
  });

  it("New Session click calls onCreateSession with the row's repo path", () => {
    // main_repo_path wins over project_path, matching handleCreateSession's
    // own project key (`main_repo_path || project_path`), so the wizard
    // prefills from the right-clicked session's project (issue #2023).
    const ws = workspace("w-new", [session({ id: "sess-new", project_path: "/p", main_repo_path: "/repos/work" })]);
    const onCreateSession = vi.fn();
    render(
      <Wrap>
        <Row ws={ws} onCreateSession={onCreateSession} />
      </Wrap>,
    );
    fireEvent.contextMenu(screen.getByTestId("sidebar-session-row"));
    fireEvent.click(screen.getByTestId("sidebar-context-menu-new-session"));
    expect(onCreateSession).toHaveBeenCalledWith("/repos/work");
    // It's a client-side wizard open, not a server mutation.
    expect(fetchSpy).not.toHaveBeenCalled();
  });

  it("hides New Session in read-only mode", () => {
    const ws = workspace("w-ro", [session({ id: "sess-ro", project_path: "/p" })]);
    render(
      <Wrap>
        <Row ws={ws} readOnly onCreateSession={vi.fn()} />
      </Wrap>,
    );
    fireEvent.contextMenu(screen.getByTestId("sidebar-session-row"));
    expect(screen.queryByTestId("sidebar-context-menu-new-session")).toBeNull();
  });
});

describe("SessionRow unread", () => {
  it("renders the unread dot for an unread row", () => {
    const ws = workspace("w-unread", [session({ id: "s-u", unread: true })]);
    render(
      <Wrap>
        <Row ws={ws} />
      </Wrap>,
    );
    expect(screen.queryByTestId("sidebar-unread-dot")).not.toBeNull();
  });

  it("suppresses the unread dot on the active row (opening reads it)", () => {
    const ws = workspace("w-unread", [session({ id: "s-u", unread: true })]);
    render(
      <Wrap>
        <Row ws={ws} isActive />
      </Wrap>,
    );
    expect(screen.queryByTestId("sidebar-unread-dot")).toBeNull();
  });

  it("menu offers 'Mark as unread' for a read row and 'Mark as read' for an unread row", () => {
    const read = workspace("w-read", [session({ id: "s-read" })]);
    const { unmount } = render(
      <Wrap>
        <Row ws={read} />
      </Wrap>,
    );
    fireEvent.contextMenu(screen.getByTestId("sidebar-session-row"));
    expect(screen.getByTestId("sidebar-context-menu-unread").textContent).toContain("Mark as unread");
    unmount();

    const unread = workspace("w-unread", [session({ id: "s-unread", unread: true })]);
    render(
      <Wrap>
        <Row ws={unread} />
      </Wrap>,
    );
    fireEvent.contextMenu(screen.getByTestId("sidebar-session-row"));
    expect(screen.getByTestId("sidebar-context-menu-unread").textContent).toContain("Mark as read");
  });

  it("'Mark as unread' fires PATCH /api/sessions/:id/unread with { unread: true } and shows the dot", async () => {
    const ws = workspace("w-live", [session({ id: "sess-unread-it" })]);
    render(
      <Wrap>
        <Row ws={ws} />
      </Wrap>,
    );
    fireEvent.contextMenu(screen.getByTestId("sidebar-session-row"));
    fireEvent.click(screen.getByTestId("sidebar-context-menu-unread"));
    // Optimistic dot appears immediately, before the PATCH round-trips.
    await vi.waitFor(() => expect(screen.queryByTestId("sidebar-unread-dot")).not.toBeNull());
    await vi.waitFor(() => expect(fetchSpy).toHaveBeenCalled());
    const [url, init] = fetchSpy.mock.calls[0]!;
    expect(url).toBe("/api/sessions/sess-unread-it/unread");
    expect(init?.method).toBe("PATCH");
    expect(JSON.parse(init!.body as string)).toEqual({ unread: true });
  });

  it("'Mark as read' on an unread row fires { unread: false }", async () => {
    const ws = workspace("w-unread", [session({ id: "sess-read-it", unread: true })]);
    render(
      <Wrap>
        <Row ws={ws} />
      </Wrap>,
    );
    fireEvent.contextMenu(screen.getByTestId("sidebar-session-row"));
    fireEvent.click(screen.getByTestId("sidebar-context-menu-unread"));
    await vi.waitFor(() => expect(fetchSpy).toHaveBeenCalled());
    const [url, init] = fetchSpy.mock.calls[0]!;
    expect(url).toBe("/api/sessions/sess-read-it/unread");
    expect(JSON.parse(init!.body as string)).toEqual({ unread: false });
  });

  it("hides the unread dot and menu item when the feature is disabled", () => {
    const ws = workspace("w-unread", [session({ id: "s-off", unread: true })]);
    render(
      <Wrap>
        <UnreadIndicatorContext.Provider value={false}>
          <Row ws={ws} />
        </UnreadIndicatorContext.Provider>
      </Wrap>,
    );
    expect(screen.queryByTestId("sidebar-unread-dot")).toBeNull();
    fireEvent.contextMenu(screen.getByTestId("sidebar-session-row"));
    expect(screen.queryByTestId("sidebar-context-menu-unread")).toBeNull();
  });
});

describe("SessionRow color label (#2383)", () => {
  it("renders the color dot when a session carries a color", () => {
    const ws = workspace("w-color", [session({ id: "s-color", color: "red" })]);
    render(
      <Wrap>
        <Row ws={ws} />
      </Wrap>,
    );
    const dot = screen.getByTestId("sidebar-session-color-dot");
    expect(dot.getAttribute("data-color")).toBe("red");
    expect(dot.className).toContain("bg-red-500");
  });

  it("renders no color dot when color is unset", () => {
    const ws = workspace("w-nocolor", [session({ id: "s-nocolor" })]);
    render(
      <Wrap>
        <Row ws={ws} />
      </Wrap>,
    );
    expect(screen.queryByTestId("sidebar-session-color-dot")).toBeNull();
  });

  it("renders no color dot for an unknown color value", () => {
    const ws = workspace("w-badcolor", [session({ id: "s-bad", color: "chartreuse" })]);
    render(
      <Wrap>
        <Row ws={ws} />
      </Wrap>,
    );
    expect(screen.queryByTestId("sidebar-session-color-dot")).toBeNull();
  });

  it("Color swatch click fires PATCH /api/sessions/:id/color with { color: 'red' }", async () => {
    const ws = workspace("w-live", [session({ id: "sess-color-it" })]);
    render(
      <Wrap>
        <Row ws={ws} />
      </Wrap>,
    );
    fireEvent.contextMenu(screen.getByTestId("sidebar-session-row"));
    fireEvent.click(screen.getByTestId("sidebar-context-menu-color-red"));
    await vi.waitFor(() => expect(fetchSpy).toHaveBeenCalled());
    const [url, init] = fetchSpy.mock.calls[0]!;
    expect(url).toBe("/api/sessions/sess-color-it/color");
    expect(init?.method).toBe("PATCH");
    expect(JSON.parse(init!.body as string)).toEqual({ color: "red" });
  });

  it("shows a Clear color item on a colored row that fires { color: null }", async () => {
    const ws = workspace("w-colored", [session({ id: "sess-clear-it", color: "green" })]);
    render(
      <Wrap>
        <Row ws={ws} />
      </Wrap>,
    );
    fireEvent.contextMenu(screen.getByTestId("sidebar-session-row"));
    fireEvent.click(screen.getByTestId("sidebar-context-menu-color-clear"));
    await vi.waitFor(() => expect(fetchSpy).toHaveBeenCalled());
    const [url, init] = fetchSpy.mock.calls[0]!;
    expect(url).toBe("/api/sessions/sess-clear-it/color");
    expect(JSON.parse(init!.body as string)).toEqual({ color: null });
  });

  it("hides the Clear color item when no color is set", () => {
    const ws = workspace("w-nocolor", [session({ id: "sess-noclear" })]);
    render(
      <Wrap>
        <Row ws={ws} />
      </Wrap>,
    );
    fireEvent.contextMenu(screen.getByTestId("sidebar-session-row"));
    expect(screen.queryByTestId("sidebar-context-menu-color-clear")).toBeNull();
  });

  it("hides the color section in read-only mode", () => {
    const ws = workspace("w-ro", [session({ id: "sess-ro-color", color: "amber" })]);
    render(
      <Wrap>
        <Row ws={ws} readOnly />
      </Wrap>,
    );
    fireEvent.contextMenu(screen.getByTestId("sidebar-session-row"));
    expect(screen.queryByTestId("sidebar-context-menu-color-red")).toBeNull();
    expect(screen.queryByTestId("sidebar-context-menu-color-clear")).toBeNull();
  });
});
