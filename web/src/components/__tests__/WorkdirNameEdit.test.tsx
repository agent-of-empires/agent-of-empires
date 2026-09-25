// @vitest-environment jsdom

import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { cleanup, fireEvent, screen } from "@testing-library/react";

import { reportError, reportInfo } from "../../lib/toastBus";
import type { SessionResponse } from "../../lib/types";
import { firstRequest, jsonResponse, makeSession, makeWorkspace, openRowMenu, stubFetch } from "./fixtures";

vi.mock("../../lib/toastBus", () => ({ reportError: vi.fn(), reportInfo: vi.fn() }));

const managed = (over: Partial<SessionResponse> = {}) =>
  makeWorkspace("w", [makeSession({ branch: "old-name", has_managed_worktree: true, ...over })], {
    branch: "old-name",
  });

let fetchSpy: ReturnType<typeof stubFetch>;
beforeEach(() => {
  vi.mocked(reportError).mockClear();
  vi.mocked(reportInfo).mockClear();
  fetchSpy = stubFetch();
});
afterEach(() => {
  cleanup();
  vi.unstubAllGlobals();
});

function rename(title: string, over: Partial<SessionResponse> = {}) {
  openRowMenu(managed(over));
  fireEvent.click(screen.getByTestId("sidebar-context-menu-rename"));
  const input = screen.getByTestId("sidebar-rename-input");
  fireEvent.change(input, { target: { value: title } });
  fireEvent.keyDown(input, { key: "Enter" });
}

function editWorkdir(name: string, renameBranch = false) {
  openRowMenu(managed());
  fireEvent.click(screen.getByTestId("sidebar-context-menu-edit-workdir"));
  fireEvent.change(screen.getByTestId("workdir-modal-name"), { target: { value: name } });
  if (renameBranch) fireEvent.click(screen.getByTestId("workdir-modal-rename-branch"));
  fireEvent.click(screen.getByTestId("workdir-modal-save"));
}

describe("sidebar Edit workdir name", () => {
  it.each([
    ["a managed, idle worktree", {}, true],
    ["a non-managed worktree", { has_managed_worktree: false }, false],
    ["a running session", { status: "Running" }, false],
    // Tied mode folds naming into Rename (#1927).
    ["a tied session", { tie_workdir_to_name: true }, false],
  ] as [string, Partial<SessionResponse>, boolean][])("on %s: offered=%s", (_n, over, offered) => {
    openRowMenu(managed(over));
    expect(screen.queryByTestId("sidebar-context-menu-edit-workdir") != null).toBe(offered);
  });

  it("PATCHes the worktree-name endpoint with name and rename_branch", async () => {
    editWorkdir("fresh-name", true);
    await vi.waitFor(() => expect(fetchSpy).toHaveBeenCalled());
    expect(firstRequest(fetchSpy)).toEqual({
      url: "/api/sessions/s1/worktree-name",
      method: "PATCH",
      body: { name: "fresh-name", rename_branch: true },
    });
  });

  it("surfaces the server validation message on failure", async () => {
    fetchSpy.mockImplementation(async () => jsonResponse({ message: "Branch 'x' already exists" }, 409));
    editWorkdir("x");
    await vi.waitFor(() => expect(screen.getByTestId("workdir-modal-error").textContent).toContain("already exists"));
  });
});

describe("sidebar inline rename", () => {
  it("PATCHes the title endpoint", async () => {
    rename("new title");
    await vi.waitFor(() => expect(fetchSpy).toHaveBeenCalled());
    expect(firstRequest(fetchSpy)).toEqual({ url: "/api/sessions/s1", method: "PATCH", body: { title: "new title" } });
  });

  it("reports warnings from a successful rename as info, not errors", async () => {
    const warning = "Session was saved, but its live tmux session could not be rekeyed";
    fetchSpy.mockResolvedValueOnce(jsonResponse({ id: "s1", warnings: [warning] }));
    rename("new title");
    await vi.waitFor(() => expect(reportInfo).toHaveBeenCalledWith(warning));
    expect(reportError).not.toHaveBeenCalled();
  });

  it.each([
    [
      "the server message for a rejected tied rename (#1927)",
      () => jsonResponse({ error: "session_running", message: "Stop the session before renaming it." }, 409),
      "Stop the session before renaming it.",
    ],
    [
      "a fallback without a server message",
      () => Promise.reject(new Error("network unavailable")),
      "Could not rename this session. Please try again.",
    ],
  ] as [string, () => Response | Promise<Response>, string][])("reports %s", async (_n, respond, message) => {
    fetchSpy.mockImplementation(async () => respond());
    rename("blocked", { tie_workdir_to_name: true });
    await vi.waitFor(() => expect(reportError).toHaveBeenCalledWith(message));
  });
});
