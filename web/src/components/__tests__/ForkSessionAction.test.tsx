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
  vi.unstubAllGlobals();
});

describe("SessionRow Fork session", () => {
  it("is hidden unless the row is structured, fork-capable, writable, and has a captured id", () => {
    for (const [over, options] of [
      [{ view: "structured", acp_can_fork: true }, {}],
      // A resume-only agent mints an id but cannot session/fork.
      [{ ...forkable, acp_can_fork: false }, {}],
      [{ view: "terminal" }, {}],
      [forkable, { readOnly: true }],
    ] as [Partial<SessionResponse>, { readOnly?: boolean }][]) {
      openRowMenu(ws(over), options);
      expect(screen.queryByTestId("sidebar-context-menu-fork")).toBeNull();
      cleanup();
    }
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
