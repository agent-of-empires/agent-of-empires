// @vitest-environment jsdom

import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { cleanup, fireEvent, screen } from "@testing-library/react";

import type { SessionResponse } from "../../lib/types";
import { OPEN_SESSION_EVENT } from "../../lib/sessionRoute";
import { OPEN_SWITCH_AGENT_EVENT, consumePendingSwitchAgent } from "../../lib/switchAgentTrigger";
import { firstRequest, makeSession, makeWorkspace, openRowMenu, renderRow, stubFetch } from "./fixtures";

const ws = (over: Partial<SessionResponse> = {}) => makeWorkspace("w", [makeSession(over)]);
const PAST = "2026-01-01T00:00:00Z";
const inMinutes = (m: number) => new Date(Date.now() + m * 60_000).toISOString();
const label = (text: string | RegExp) => screen.queryByLabelText(text);
const testId = (id: string) => screen.queryByTestId(id);
const click = (id: string) => fireEvent.click(screen.getByTestId(id));

let fetchSpy: ReturnType<typeof stubFetch>;
beforeEach(() => {
  fetchSpy = stubFetch();
});
afterEach(() => {
  cleanup();
  vi.unstubAllGlobals();
  consumePendingSwitchAgent("sess-switch-it");
});

describe("SessionRow chips", () => {
  const chipCases = [
    ["pinned", { pinned_at: PAST }, ["Pinned"], ["Archived", "Snoozed"]],
    ["archived", { archived_at: PAST }, ["Archived"], ["Pinned", "Snoozed"]],
    // Archive wins visually when both flags surface.
    ["archived and snoozed", { archived_at: PAST, snoozed_until: "2099-01-01T00:00:00Z" }, ["Archived"], ["Snoozed"]],
    ["smart_rename pending", { view: "structured", smart_rename: "pending" }, ["Will auto-name"], ["Naming"]],
    ["smart_rename running", { view: "structured", smart_rename: "running" }, ["Naming"], ["Will auto-name"]],
    ["smart_rename inactive", { view: "structured", smart_rename: "inactive" }, [], ["Naming", "Will auto-name"]],
    ["worker stopping", { view: "structured", acp_worker_state: "stopping" }, ["Stopping"], []],
    ["armed monitor", { monitor_active: true, monitor_description: "clippy passes" }, ["Monitoring clippy passes"], []],
    ["no monitor", {}, [], [/^Monitoring/]],
  ] as [string, Partial<SessionResponse>, (string | RegExp)[], (string | RegExp)[]][];
  it("shows chips for row state", () => {
    for (const [name, over, present, absent] of chipCases) {
      renderRow(ws(over));
      for (const l of present) expect(label(l), name).not.toBeNull();
      for (const l of absent) expect(label(l), name).toBeNull();
      cleanup();
    }
  });

  it("shows the snooze remaining time and the payload rate-limit park", () => {
    renderRow(ws({ snoozed_until: inMinutes(90) }));
    expect(label("Snoozed")!.textContent).toMatch(/1h/);
    cleanup();
    renderRow(
      ws({ view: "structured", rate_limit: { status: "limited", resets_at: "2099-01-01T00:00:00Z", kind: "usage" } }),
    );
    expect(screen.getByTitle(/Rate-limited/)).not.toBeNull();
  });
});

describe("SessionRow row tags", () => {
  it("renders a compact branch tag instead of the branch subtitle, and nothing for none", () => {
    const branched = makeWorkspace("w", [makeSession({ branch: "feature/web-row-tag" })], {
      branch: "feature/web-row-tag",
    });
    const { rerenderRow } = renderRow(branched, { rowTagMode: "branch" });
    expect(testId("sidebar-session-row-tag")!.textContent).toBe("[web-row-tag]");
    expect(screen.queryByText("feature/web-row-tag")).toBeNull();
    rerenderRow(branched, { rowTagMode: "none" });
    expect(testId("sidebar-session-row-tag")).toBeNull();
    expect(screen.queryByText("feature/web-row-tag")).toBeNull();
  });

  it("renders profile, auto, and sandbox tags from the first session", () => {
    for (const [mode, tag] of [
      ["profile", "[fb]"],
      ["auto", "[fb]"],
      ["sandbox", "[sb]"],
    ] as const) {
      renderRow(ws({ profile: "forit-backup", is_sandboxed: true }), { rowTagMode: mode });
      expect(testId("sidebar-session-row-tag")!.textContent, mode).toBe(tag);
      cleanup();
    }
  });

  it("keeps repo chips beside a multi-repo branch tag", () => {
    renderRow(
      ws({
        workspace_repos: [
          { name: "api", source_path: "/repo/api", branch: "feature/web-tags" },
          { name: "web", source_path: "/repo/web", branch: "feature/web-tags" },
        ],
      }),
    );
    expect(testId("sidebar-session-row-tag")!.textContent).toBe("[web-tags+2]");
    expect(screen.getByText("api")).not.toBeNull();
    expect(screen.getByText("web")).not.toBeNull();
  });
});

describe("SessionRow unread", () => {
  it("shows the dot only on live, inactive rows with the feature on (#2571)", () => {
    for (const [name, over, options, shown] of [
      ["live idle unread row", {}, {}, true],
      ["archived row (#2571)", { archived_at: PAST }, {}, false],
      ["snoozed row (#2571)", { snoozed_until: inMinutes(90) }, {}, false],
      ["active row", {}, { isActive: true }, false],
      ["disabled feature", {}, { unread: false }, false],
    ] as [string, Partial<SessionResponse>, object, boolean][]) {
      renderRow(ws({ unread: true, ...over }), { unread: true, ...options });
      expect(testId("sidebar-unread-dot") != null, name).toBe(shown);
      cleanup();
    }
    openRowMenu(ws({ unread: true }), { unread: false });
    expect(testId("sidebar-context-menu-unread")).toBeNull();
  });

  it("toggles unread optimistically and PATCHes the new value", async () => {
    for (const [unread, text, next] of [
      [false, "Mark as unread", true],
      [true, "Mark as read", false],
    ] as const) {
      fetchSpy.mockClear();
      openRowMenu(ws({ id: "sess-u", unread }));
      expect(testId("sidebar-context-menu-unread")!.textContent).toContain(text);
      click("sidebar-context-menu-unread");
      // The dot flips optimistically before the PATCH lands.
      await vi.waitFor(() => expect(testId("sidebar-unread-dot") != null).toBe(next));
      await vi.waitFor(() => expect(fetchSpy).toHaveBeenCalled());
      expect(firstRequest(fetchSpy)).toEqual({
        url: "/api/sessions/sess-u/unread",
        method: "PATCH",
        body: { unread: next },
      });
      cleanup();
    }
  });
});

describe("SessionRow context menu", () => {
  it("offers triage items by row state and Switch agent only on structured rows", () => {
    for (const [name, over, has, lacks] of [
      // Archiving or snoozing a pinned session clears the pin server-side, as in the TUI.
      ["pinned", { pinned_at: PAST }, ["Unpin", "Archive", "Snooze"], []],
      ["archived", { archived_at: PAST }, ["Unarchive"], ["Pin", "Snooze"]],
      ["snoozed", { snoozed_until: inMinutes(60) }, ["Unsnooze"], ["Pin", "Archive"]],
      ["live", {}, ["Pin", "Archive", "Snooze…"], []],
    ] as [string, Partial<SessionResponse>, string[], string[]][]) {
      const text = openRowMenu(ws(over)).textContent;
      for (const t of has) expect(text, name).toContain(t);
      for (const t of lacks) expect(text, name).not.toContain(t);
      cleanup();
    }
    openRowMenu(ws({ view: "structured" }));
    expect(testId("sidebar-context-menu-switch-agent")).not.toBeNull();
    cleanup();
    openRowMenu(ws({ view: "terminal" }));
    expect(testId("sidebar-context-menu-switch-agent")).toBeNull();
  });

  it("hides write actions in read-only mode", () => {
    const text = openRowMenu(ws({ view: "structured", color: "amber" }), {
      readOnly: true,
      onCreateSession: vi.fn(),
    }).textContent;
    for (const t of ["Pin", "Archive", "Snooze", "Delete"]) expect(text).not.toContain(t);
    for (const id of ["switch-agent", "new-session", "color-red", "color-clear"]) {
      expect(testId(`sidebar-context-menu-${id}`)).toBeNull();
    }
  });
});

describe("SessionRow triage actions", () => {
  it("each triage action PATCHes its endpoint", async () => {
    for (const [name, over, item, path, body] of [
      ["Pin", {}, "sidebar-context-menu-pin", "pin", { pinned: true }],
      ["Unpin", { pinned_at: PAST }, "sidebar-context-menu-pin", "pin", { pinned: false }],
      ["Archive", {}, "sidebar-context-menu-archive", "archive", { archived: true, kill_pane: true }],
      [
        "Unarchive",
        { archived_at: PAST },
        "sidebar-context-menu-archive",
        "archive",
        { archived: false, kill_pane: true },
      ],
      ["Unsnooze", { snoozed_until: inMinutes(60) }, "sidebar-context-menu-unsnooze", "snooze", { minutes: null }],
      ["Color", {}, "sidebar-context-menu-color-red", "color", { color: "red" }],
      ["Clear color", { color: "green" }, "sidebar-context-menu-color-clear", "color", { color: null }],
    ] as [string, Partial<SessionResponse>, string, string, unknown][]) {
      fetchSpy.mockClear();
      openRowMenu(ws({ id: "sess-it", ...over }));
      click(item);
      await vi.waitFor(() => expect(fetchSpy, name).toHaveBeenCalled());
      expect(firstRequest(fetchSpy), name).toEqual({ url: `/api/sessions/sess-it/${path}`, method: "PATCH", body });
      cleanup();
    }
  });

  it("Pin and Archive show their chip optimistically and revert it on failure", async () => {
    let fail = () => {};
    fetchSpy.mockImplementation(
      () => new Promise((resolve) => (fail = () => resolve(new Response("nope", { status: 500 })))),
    );
    for (const [item, chip] of [
      ["sidebar-context-menu-pin", "Pinned"],
      ["sidebar-context-menu-archive", "Archived"],
    ]) {
      openRowMenu(ws({ id: "sess-fail" }));
      click(item);
      // Regression: the chip must read the optimistic state, not wait for the poll.
      await vi.waitFor(() => expect(label(chip)).not.toBeNull());
      fail();
      await vi.waitFor(() => expect(label(chip)).toBeNull());
      cleanup();
    }
  });

  it("Snooze… opens the modal without a request", () => {
    openRowMenu(ws());
    click("sidebar-context-menu-snooze");
    expect(testId("snooze-modal")).not.toBeNull();
    expect(fetchSpy).not.toHaveBeenCalled();
  });

  it("Switch agent navigates to the session and requests the dialog", () => {
    const events: string[] = [];
    const record = (e: Event) => events.push(`${e.type}:${(e as CustomEvent).detail.sessionId}`);
    window.addEventListener(OPEN_SESSION_EVENT, record);
    window.addEventListener(OPEN_SWITCH_AGENT_EVENT, record);
    try {
      openRowMenu(ws({ id: "sess-switch-it", view: "structured" }));
      click("sidebar-context-menu-switch-agent");
      expect(events).toEqual([`${OPEN_SESSION_EVENT}:sess-switch-it`, `${OPEN_SWITCH_AGENT_EVENT}:sess-switch-it`]);
      expect(fetchSpy).not.toHaveBeenCalled();
    } finally {
      window.removeEventListener(OPEN_SESSION_EVENT, record);
      window.removeEventListener(OPEN_SWITCH_AGENT_EVENT, record);
    }
  });

  it("New Session opens the wizard with main_repo_path over project_path (#2023)", () => {
    const onCreateSession = vi.fn();
    openRowMenu(ws({ project_path: "/p", main_repo_path: "/repos/work" }), { onCreateSession });
    click("sidebar-context-menu-new-session");
    expect(onCreateSession).toHaveBeenCalledWith("/repos/work");
    expect(fetchSpy).not.toHaveBeenCalled();
  });
});

describe("SessionRow color label (#2383)", () => {
  it("shows a dot only for a known color while colors are enabled (#3104)", () => {
    for (const [color, options, shown] of [
      ["red", {}, "red"],
      [undefined, {}, null],
      ["chartreuse", {}, null],
      // Disabling colors hides the stored value without clearing it (#3104).
      ["red", { colorsEnabled: false }, null],
    ] as const) {
      renderRow(ws({ color }), options);
      const dot = testId("sidebar-session-color-dot");
      expect(dot?.getAttribute("data-color") ?? null, `${color} ${JSON.stringify(options)}`).toBe(shown);
      if (shown) expect(dot!.className).toContain("bg-red-500");
      cleanup();
    }
  });

  it("offers Clear only for a colored row and no color section when disabled", () => {
    openRowMenu(ws());
    expect(testId("sidebar-context-menu-color-clear")).toBeNull();
    cleanup();
    openRowMenu(ws({ color: "green" }), { colorsEnabled: false });
    for (const key of ["red", "amber", "green", "clear"]) {
      expect(testId(`sidebar-context-menu-color-${key}`)).toBeNull();
    }
  });
});
