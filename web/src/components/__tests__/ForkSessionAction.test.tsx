// @vitest-environment jsdom

import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { cleanup, fireEvent, screen } from "@testing-library/react";

import type { SessionResponse } from "../../lib/types";
import { firstRequest, makeSession, makeWorkspace, openRowMenu, stubFetch } from "./fixtures";

const forkable = { view: "structured", acp_session_id: "acp-parent", acp_can_fork: true } as const;
const ws = (over: Partial<SessionResponse>) => makeWorkspace("w", [makeSession(over)]);

let fetchSpy: ReturnType<typeof stubFetch>;
beforeEach(() => {
  fetchSpy = stubFetch();
});
afterEach(() => {
  cleanup();
  vi.unstubAllGlobals();
});

describe("SessionRow Fork session", () => {
  it.each([
    ["a structured row with no captured acp_session_id", { view: "structured", acp_can_fork: true }, {}],
    // A resume-only agent mints an id but cannot session/fork.
    ["a resume-only row", { ...forkable, acp_can_fork: false }, {}],
    ["a terminal row", { view: "terminal" }, {}],
    ["a read-only forkable row", forkable, { readOnly: true }],
  ] as [string, Partial<SessionResponse>, { readOnly?: boolean }][])("is hidden on %s", (_n, over, options) => {
    openRowMenu(ws(over), options);
    expect(screen.queryByTestId("sidebar-context-menu-fork")).toBeNull();
  });

  it("offers fork on a structured, fork-capable row and POSTs a structured create with fork_from", async () => {
    openRowMenu(ws({ ...forkable, project_path: "/repo", profile: "work", acp_session_id: "acp-parent-42" }));
    fireEvent.click(screen.getByTestId("sidebar-context-menu-fork"));
    await vi.waitFor(() => expect(fetchSpy).toHaveBeenCalled());
    expect(firstRequest(fetchSpy)).toEqual({
      url: "/api/sessions",
      method: "POST",
      body: { path: "/repo", tool: "claude", view: "structured", profile: "work", fork_from: "acp-parent-42" },
    });
  });
});
