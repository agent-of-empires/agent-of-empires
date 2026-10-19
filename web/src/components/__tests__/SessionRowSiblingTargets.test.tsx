// @vitest-environment jsdom

import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { cleanup, fireEvent, screen } from "@testing-library/react";

import type { SessionResponse } from "../../lib/types";
import { EMPTY_OPTIMISTIC } from "../../lib/sidebarOptimistic";
import { canTarget, resolveActionTargets } from "../sidebar/sessionActionTargets";
import { makeSession, makeWorkspace, openRowMenu, stubFetch } from "./fixtures";

const sibling = (id: string, over: Partial<SessionResponse> = {}) => makeSession({ id, title: `title ${id}`, ...over });
const structured = { view: "structured", acp_session_id: "acp", acp_can_fork: true } as const;
const ids = (list: SessionResponse[]) => list.map((s) => s.id);
const click = (id: string) => fireEvent.click(screen.getByTestId(id));

beforeEach(() => {
  stubFetch();
});
afterEach(() => {
  cleanup();
  vi.unstubAllGlobals();
});

describe("resolveActionTargets", () => {
  it("picks eligible siblings per action in row order", () => {
    const sessions = [
      sibling("stopped", { status: "Stopped" }),
      sibling("running", { status: "Running" }),
      sibling("archived", { status: "Stopped", archived_at: "2026-01-01T00:00:00Z" }),
      sibling("trashed", { status: "Stopped", trashed_at: "2026-01-01T00:00:00Z" }),
      sibling("creating", { status: "Creating" }),
      sibling("acp", { ...structured, acp_can_fork: false }),
      sibling("fork", structured),
      sibling("capable", { view: "terminal", acp_capable: true }),
    ];
    const t = resolveActionTargets(sessions, EMPTY_OPTIMISTIC);
    expect(ids(t.stop)).toEqual(["running", "acp", "fork", "capable"]);
    expect(ids(t.start)).toEqual(["stopped"]);
    expect(ids(t.fork)).toEqual(["fork"]);
    expect(ids(t.summarize)).toEqual(["acp", "fork"]);
    expect(ids(t.switchView)).toEqual(["acp", "fork", "capable"]);
  });

  it("hides Start while an archive is pending, and allows it once unarchiving", () => {
    const stopped = [sibling("a", { status: "Stopped" }), sibling("b", { status: "Stopped", archived_at: "x" })];
    expect(ids(resolveActionTargets(stopped, { ...EMPTY_OPTIMISTIC, archived: true }).start)).toEqual([]);
    expect(ids(resolveActionTargets(stopped, { ...EMPTY_OPTIMISTIC, archived: false }).start)).toEqual(["a", "b"]);
  });

  it("canTarget rejects a session that left the action's state", () => {
    expect(canTarget("stop", sibling("a", { status: "Running" }))).toBe(true);
    expect(canTarget("stop", sibling("a", { status: "Stopped" }))).toBe(false);
  });
});

describe("shared-worktree row session actions (#3387)", () => {
  it("Stop and Start act on the sibling that qualifies, not sessions[0]", () => {
    const onStop = vi.fn();
    const onStart = vi.fn();
    openRowMenu(makeWorkspace("w", [sibling("s1", { status: "Stopped" }), sibling("s2", { status: "Running" })]), {
      onStop,
      onStart,
    });
    expect(screen.queryByTestId("sidebar-context-menu-stop-target")).toBeNull();
    click("sidebar-context-menu-stop");
    expect(onStop).toHaveBeenCalledWith("s2");
  });

  it("Start is offered for a stopped sibling while another runs", () => {
    const onStart = vi.fn();
    openRowMenu(makeWorkspace("w", [sibling("s1", { status: "Running" }), sibling("s2", { status: "Stopped" })]), {
      onStart,
    });
    click("sidebar-context-menu-start");
    expect(onStart).toHaveBeenCalledWith("s2");
  });

  it("asks which sibling when several qualify, and acts on the chosen one only", () => {
    const onStop = vi.fn();
    openRowMenu(
      makeWorkspace("w", [
        sibling("s1", { status: "Running" }),
        sibling("s2", { status: "Waiting" }),
        sibling("s3", { status: "Stopped" }),
      ]),
      { onStop },
    );
    click("sidebar-context-menu-stop");
    expect(onStop).not.toHaveBeenCalled();
    const rows = screen.getAllByTestId("sidebar-context-menu-stop-target");
    expect(rows.map((r) => r.textContent)).toEqual(["title s1Running", "title s2Waiting"]);
    fireEvent.click(rows[1]!);
    expect(onStop).toHaveBeenCalledTimes(1);
    expect(onStop).toHaveBeenCalledWith("s2");
  });

  it("Summarize targets the chosen structured sibling, not the first", async () => {
    const fetchSpy = stubFetch();
    openRowMenu(makeWorkspace("w", [sibling("s1", structured), sibling("s2", structured)]));
    click("sidebar-context-menu-summarize");
    fireEvent.click(screen.getAllByTestId("sidebar-context-menu-summarize-target")[1]!);
    await vi.waitFor(() => expect(fetchSpy).toHaveBeenCalled());
    expect(fetchSpy.mock.calls[0]![0]).toContain("s2");
  });
});
