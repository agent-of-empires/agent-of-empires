// @vitest-environment jsdom
// The Session menu's tiers (#5074): order and separators across row states, plus labels and hints.

import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { cleanup, screen } from "@testing-library/react";

import type { SessionResponse } from "../../lib/types";
import { makeSession, makeWorkspace, openRowMenu, stubFetch } from "./fixtures";

beforeEach(() => stubFetch());
afterEach(() => {
  cleanup();
  vi.unstubAllGlobals();
});

/** The menu's items in DOM order by test id, with "|" for each separator. */
function order() {
  const out: string[] = [];
  for (const el of screen
    .getByTestId("sidebar-context-menu")
    .querySelectorAll<HTMLElement>('[data-testid^="sidebar-context-menu-"], div.border-t.my-1')) {
    const id = el.dataset.testid?.replace("sidebar-context-menu-", "");
    if (id === undefined) out.push("|");
    else if (id.endsWith("-target") || id === "summary" || id === "close") continue;
    else {
      const token = id.startsWith("notify-") ? "notify" : id.startsWith("color-") ? "color" : id;
      if (out.at(-1) !== token) out.push(token);
    }
  }
  return out.join(" ");
}

const open = (sessions: Partial<SessionResponse>[], options: object = {}) =>
  openRowMenu(
    makeWorkspace(
      "w",
      sessions.map((s) => makeSession(s)),
    ),
    options,
  );

describe("Session menu tiers", () => {
  it("orders items by tier, with separators only between tiers that show something", () => {
    const quick = "pin archive snooze unread";
    const tail = "notify color | delete";
    // (state, sessions, options, header line, order with More open)
    const cases: [string, Partial<SessionResponse>[], object, string, string][] = [
      [
        "idle",
        [{ status: "Idle" }],
        {},
        "Idle · Terminal view",
        `${quick} | rename | stop | more edit-group add-project | ${tail}`,
      ],
      [
        "stopped",
        [{ status: "Stopped" }],
        {},
        "Stopped · Terminal view",
        `${quick} | rename | start | more edit-group add-project | ${tail}`,
      ],
      // Restart to update leads the Run tier.
      [
        "update waiting",
        [{ status: "Idle", update_restart: true }],
        {},
        "Idle · Terminal view",
        `${quick} | rename | restart-update stop | more edit-group add-project | ${tail}`,
      ],
      // Recreate container sits last in More, after its own separator.
      [
        "sandboxed",
        [{ status: "Running", is_sandboxed: true }],
        {},
        "Running · Terminal view",
        `${quick} | rename | stop | more edit-group add-project | recreate-container | ${tail}`,
      ],
      [
        "structured, can fork",
        [{ status: "Idle", view: "structured", acp_session_id: "x", acp_can_fork: true }],
        {},
        "Idle · Structured view",
        `${quick} | rename | stop switch-view | fork | more summarize auto-name switch-agent | edit-group add-project | ${tail}`,
      ],
      // Several Sessions: a count, and the targeted items keep their pickers.
      [
        "multi-Session",
        [
          { id: "a", status: "Idle" },
          { id: "b", status: "Running" },
        ],
        {},
        "2 Sessions",
        `${quick} | rename | stop | more edit-group add-project | ${tail}`,
      ],
      // Read-only hides every write action and leaves no stray separator.
      ["read-only", [{ status: "Idle" }], { readOnly: true }, "Idle · Terminal view", "notify"],
    ];
    for (const [state, sessions, options, header, expected] of cases) {
      open(sessions, options);
      expect(screen.getByTestId("sidebar-context-menu-summary").textContent, state).toBe(header);
      expect(order(), state).toBe(expected);
      cleanup();
    }
  });

  it("names the regrouped items, with second lines as their descriptions", () => {
    // (Session, [name, description] it offers)
    const cases: [Partial<SessionResponse>, [string, string?][]][] = [
      [
        { status: "Running", update_restart: true, is_sandboxed: true },
        [
          ["Restart to update", "Keeps the conversation"],
          ["Recreate container", "Resets anything installed inside it"],
          ["Move to another group"],
          ["Move to Trash…"],
        ],
      ],
      [{ status: "Stopped", has_managed_worktree: true }, [["Rename workdir"]]],
    ];
    for (const [session, items] of cases) {
      open([session]);
      for (const [name, description] of items) {
        expect(screen.getByRole("button", { name, ...(description ? { description } : {}) }), name).toBeTruthy();
      }
      for (const old of ["Restart to apply update (keeps conversation)", "Edit group", "Edit workdir name", "Delete"]) {
        expect(screen.queryByRole("button", { name: old }), old).toBeNull();
      }
      cleanup();
    }
  });

  it("shows the Notify hint only when it isn't Default", () => {
    const flags = (on: boolean) => ({ notify_on_waiting: on, notify_on_idle: on, notify_on_error: on });
    // (notify flags, hint shown)
    const cases: [Partial<SessionResponse>, string | null][] = [
      [{}, null],
      [flags(false), "No notifications from this session"],
      [flags(true), "Notifies when waiting, idle, or on error"],
    ];
    for (const [over, hint] of cases) {
      open([{ status: "Idle", ...over }]);
      const text = screen.getByTestId("sidebar-context-menu").textContent ?? "";
      expect(text.includes("Follows your notification settings"), JSON.stringify(over)).toBe(false);
      if (hint) expect(text, JSON.stringify(over)).toContain(hint);
      cleanup();
    }
  });
});
