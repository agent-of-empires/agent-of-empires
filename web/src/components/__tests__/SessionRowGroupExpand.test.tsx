// @vitest-environment jsdom

import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { cleanup, fireEvent, screen } from "@testing-library/react";

import type { RowBulkApi } from "../sidebar/types";
import { makeSession, makeWorkspace, renderRow, stubFetch } from "./fixtures";

const PAST = "2026-01-01T00:00:00Z";
const expand = () => fireEvent.click(screen.getByTestId("sidebar-group-expand-toggle"));
const rows = () => screen.getAllByTestId("sidebar-session-row");

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

  it("names the toggle for what it expands, not just the count", () => {
    renderRow(makeWorkspace("w", [makeSession({ id: "s1" }), makeSession({ id: "s2" })]));
    expect(screen.getByRole("button", { name: "Sessions (2)" }).getAttribute("aria-expanded")).toBe("false");
  });

  it("expands into one full, indented row per session and collapses back", () => {
    renderRow(
      makeWorkspace("w", [
        makeSession({ id: "s1", title: "first" }),
        makeSession({ id: "s2", title: "second", status: "Running" }),
      ]),
    );
    expect(rows()).toHaveLength(1);

    const toggle = screen.getByTestId("sidebar-group-expand-toggle");
    expect(toggle.textContent).toContain("2");
    fireEvent.click(toggle);

    expect(rows()).toHaveLength(3);
    expect(rows()[1]!.textContent).toContain("first");
    expect(rows()[2]!.textContent).toContain("second");
    // The group header keeps its own padding; its children get the indented row's.
    expect(rows()[0]!.className).not.toContain("pl-6");
    expect(rows()[1]!.className).toContain("pl-6");
    expect(rows()[2]!.className).toContain("pl-6");

    expand();
    expect(rows()).toHaveLength(1);
  });

  it("hides expanded rows in the compact rail, where the toggle is gone", () => {
    const ws = makeWorkspace("w", [makeSession({ id: "s1" }), makeSession({ id: "s2" })]);
    const { rerenderRow } = renderRow(ws);
    expand();
    expect(rows()).toHaveLength(3);

    rerenderRow(ws, { compact: true });
    expect(rows()).toHaveLength(1);
  });

  it("navigates to the specific sibling clicked, not the group's default session", () => {
    const onActivate = vi.fn();
    renderRow(makeWorkspace("w", [makeSession({ id: "s1" }), makeSession({ id: "s2" })]), { onActivate });
    expand();

    fireEvent.click(rows()[2]!);
    expect(onActivate).toHaveBeenCalledTimes(1);
    expect(onActivate.mock.calls[0]![0]).toBe("w");
    expect(onActivate.mock.calls[0]![2]).toBe("s2");
  });

  it("navigates on a modified click instead of toggling the group's selection", () => {
    const onActivate = vi.fn();
    renderRow(makeWorkspace("w", [makeSession({ id: "s1" }), makeSession({ id: "s2" })]), { onActivate });
    expand();

    fireEvent.click(rows()[2]!, { ctrlKey: true, shiftKey: true });
    expect(onActivate).toHaveBeenCalledWith("w", { metaKey: false, ctrlKey: false, shiftKey: false }, "s2");
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
    expand();
    fireEvent.contextMenu(rows()[2]!);

    // A single-session row has exactly one target, so Stop runs directly with no sibling disclosure.
    expect(screen.queryByTestId("sidebar-context-menu-stop-target")).toBeNull();
    fireEvent.click(screen.getByTestId("sidebar-context-menu-stop"));
    expect(onStop).toHaveBeenCalledTimes(1);
    expect(onStop).toHaveBeenCalledWith("s2");
  });

  it("never consults the group's selection for a sibling's menu", () => {
    const bulkApi: RowBulkApi = {
      prepareScope: vi.fn(() => ({ kind: "single" as const })),
      pin: vi.fn(),
      archive: vi.fn(),
      snooze: vi.fn(),
    };
    renderRow(makeWorkspace("w", [makeSession({ id: "s1" }), makeSession({ id: "s2" })]), { bulkApi });
    expand();

    fireEvent.contextMenu(rows()[0]!);
    expect(bulkApi.prepareScope).toHaveBeenCalledTimes(1);
    expect(vi.mocked(bulkApi.prepareScope).mock.calls[0]![0].id).toBe("w");

    fireEvent.contextMenu(rows()[2]!);
    expect(bulkApi.prepareScope).toHaveBeenCalledTimes(1);
  });

  it("keeps a sibling's pin change off the group header and the other sessions", async () => {
    renderRow(
      makeWorkspace("w", [
        makeSession({ id: "s1", title: "first", pinned_at: PAST }),
        makeSession({ id: "s2", title: "second", pinned_at: PAST }),
      ]),
    );
    expand();
    expect(screen.getAllByTestId("sidebar-pinned-mark")).toHaveLength(3);

    fireEvent.contextMenu(rows()[2]!);
    fireEvent.click(screen.getByTestId("sidebar-context-menu-pin"));

    // s2 unpins; the header and s1 stay pinned, since s1 still is.
    await vi.waitFor(() => expect(screen.getAllByTestId("sidebar-pinned-mark")).toHaveLength(2));
    expect(rows()[0]!.querySelector("[data-testid=sidebar-pinned-mark]")).not.toBeNull();
    expect(rows()[1]!.querySelector("[data-testid=sidebar-pinned-mark]")).not.toBeNull();
    expect(rows()[2]!.querySelector("[data-testid=sidebar-pinned-mark]")).toBeNull();
  });

  it("marks only the sibling that is actually open, not the whole group", () => {
    renderRow(makeWorkspace("w", [makeSession({ id: "s1" }), makeSession({ id: "s2" })]), {
      isActive: true,
      activeSessionId: "s2",
    });
    expand();
    expect(rows()[0]!.hasAttribute("data-active-session-row")).toBe(true); // the group header itself
    expect(rows()[1]!.hasAttribute("data-active-session-row")).toBe(false); // s1, not the open one
    expect(rows()[2]!.hasAttribute("data-active-session-row")).toBe(true); // s2, the open one
  });
});
