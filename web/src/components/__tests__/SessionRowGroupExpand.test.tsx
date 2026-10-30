// @vitest-environment jsdom

import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { cleanup, fireEvent, screen } from "@testing-library/react";

import { makeSession, makeWorkspace, renderRow, stubFetch } from "./fixtures";

beforeEach(() => {
  stubFetch();
});
afterEach(() => {
  cleanup();
  vi.unstubAllGlobals();
});

describe("expanding a shared-worktree row into its sessions", () => {
  it("renders no expand toggle for a workspace with a single session", () => {
    renderRow(makeWorkspace("w", [makeSession({ id: "s1" })]));
    expect(screen.queryByTestId("sidebar-group-expand-toggle")).toBeNull();
  });

  it("expands into one full, indented row per session and collapses back", () => {
    renderRow(
      makeWorkspace("w", [
        makeSession({ id: "s1", title: "first" }),
        makeSession({ id: "s2", title: "second", status: "Running" }),
      ]),
    );
    expect(screen.getAllByTestId("sidebar-session-row")).toHaveLength(1);

    const toggle = screen.getByTestId("sidebar-group-expand-toggle");
    expect(toggle.textContent).toContain("2");
    fireEvent.click(toggle);

    const rows = screen.getAllByTestId("sidebar-session-row");
    expect(rows).toHaveLength(3);
    expect(rows[1]!.textContent).toContain("first");
    expect(rows[2]!.textContent).toContain("second");
    // The group header keeps its own padding; its children get the indented row's.
    expect(rows[0]!.className).not.toContain("pl-6");
    expect(rows[1]!.className).toContain("pl-6");
    expect(rows[2]!.className).toContain("pl-6");

    fireEvent.click(screen.getByTestId("sidebar-group-expand-toggle"));
    expect(screen.getAllByTestId("sidebar-session-row")).toHaveLength(1);
  });

  it("navigates to the specific sibling clicked, not the group's default session", () => {
    const onActivate = vi.fn();
    renderRow(makeWorkspace("w", [makeSession({ id: "s1" }), makeSession({ id: "s2" })]), { onActivate });
    fireEvent.click(screen.getByTestId("sidebar-group-expand-toggle"));

    fireEvent.click(screen.getAllByTestId("sidebar-session-row")[2]!);
    expect(onActivate).toHaveBeenCalledTimes(1);
    expect(onActivate.mock.calls[0]![0]).toBe("w");
    expect(onActivate.mock.calls[0]![2]).toBe("s2");
  });

  it("gives a sibling row its own context menu, scoped to just that session", () => {
    const onStop = vi.fn();
    renderRow(
      makeWorkspace("w", [
        makeSession({ id: "s1", title: "first", status: "Stopped" }),
        makeSession({ id: "s2", title: "second", status: "Running" }),
      ]),
      { onStop },
    );
    fireEvent.click(screen.getByTestId("sidebar-group-expand-toggle"));
    const s2Row = screen.getAllByTestId("sidebar-session-row")[2]!;
    fireEvent.contextMenu(s2Row);

    // A single-session row has exactly one target, so Stop runs directly with no sibling disclosure.
    expect(screen.queryByTestId("sidebar-context-menu-stop-target")).toBeNull();
    fireEvent.click(screen.getByTestId("sidebar-context-menu-stop"));
    expect(onStop).toHaveBeenCalledTimes(1);
    expect(onStop).toHaveBeenCalledWith("s2");
  });

  it("marks only the sibling that is actually open, not the whole group", () => {
    renderRow(makeWorkspace("w", [makeSession({ id: "s1" }), makeSession({ id: "s2" })]), {
      isActive: true,
      activeSessionId: "s2",
    });
    fireEvent.click(screen.getByTestId("sidebar-group-expand-toggle"));
    const rows = screen.getAllByTestId("sidebar-session-row");
    expect(rows[0]!.hasAttribute("data-active-session-row")).toBe(true); // the group header itself
    expect(rows[1]!.hasAttribute("data-active-session-row")).toBe(false); // s1, not the open one
    expect(rows[2]!.hasAttribute("data-active-session-row")).toBe(true); // s2, the open one
  });

  it("names the group header after its preferred session, not the branch", () => {
    renderRow(
      makeWorkspace(
        "w",
        [
          makeSession({ id: "s1", title: "first", status: "Idle" }),
          makeSession({ id: "s2", title: "second", status: "Running" }),
        ],
        { branch: "feature/thing" },
      ),
    );
    // The running sibling is preferred over the first one, and both over the branch name.
    expect(screen.getAllByTestId("sidebar-session-row")[0]!.textContent).toContain("second");
  });

  it("falls back to the branch name when no session has its own title", () => {
    renderRow(
      makeWorkspace("w", [makeSession({ id: "s1", title: "" }), makeSession({ id: "s2", title: "" })], {
        branch: "feature/thing",
      }),
    );
    expect(screen.getAllByTestId("sidebar-session-row")[0]!.textContent).toContain("feature/thing");
  });
});
