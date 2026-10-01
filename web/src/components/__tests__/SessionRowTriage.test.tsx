// @vitest-environment jsdom

import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { act, cleanup, fireEvent, screen } from "@testing-library/react";

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
  it.each([
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
  ] as [string, Partial<SessionResponse>, (string | RegExp)[], (string | RegExp)[]][])(
    "%s",
    (_name, over, present, absent) => {
      renderRow(ws(over));
      for (const l of present) expect(label(l)).not.toBeNull();
      for (const l of absent) expect(label(l)).toBeNull();
    },
  );

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
  it.each([
    ["live idle unread row", {}, {}, true],
    ["archived row (#2571)", { archived_at: PAST }, {}, false],
    ["snoozed row (#2571)", { snoozed_until: inMinutes(90) }, {}, false],
    ["active row", {}, { isActive: true }, false],
    ["disabled feature", {}, { unread: false }, false],
  ])("dot on a %s: %s", (_name, over, options, shown) => {
    renderRow(ws({ unread: true, ...over }), { unread: true, ...options });
    expect(testId("sidebar-unread-dot") != null).toBe(shown);
  });

  it("hides the menu item when the feature is disabled", () => {
    openRowMenu(ws({ unread: true }), { unread: false });
    expect(testId("sidebar-context-menu-unread")).toBeNull();
  });

  it.each([false, true])("unread=%s toggles in place and PATCHes the opposite", async (unread) => {
    const next = !unread;
    openRowMenu(ws({ id: "sess-u", unread }));
    const toggle = () => testId("sidebar-context-menu-unread")!;
    expect(toggle().getAttribute("aria-pressed")).toBe(String(unread));
    click("sidebar-context-menu-unread");
    // The dot and the toggle flip optimistically before the PATCH lands, with the menu still open.
    await vi.waitFor(() => expect(testId("sidebar-unread-dot") != null).toBe(next));
    expect(toggle().getAttribute("aria-pressed")).toBe(String(next));
    expect(testId("sidebar-context-menu")).not.toBeNull();
    await vi.waitFor(() => expect(fetchSpy).toHaveBeenCalled());
    expect(firstRequest(fetchSpy)).toEqual({
      url: "/api/sessions/sess-u/unread",
      method: "PATCH",
      body: { unread: next },
    });
  });
});

describe("SessionRow context menu", () => {
  it("keeps the rarer actions folded under a More disclosure until it is opened", () => {
    openRowMenu(ws({ view: "structured" }), { expandMore: false });
    const more = screen.getByTestId("sidebar-context-menu-more");
    const group = document.getElementById(more.getAttribute("aria-controls")!)!;
    expect(more.getAttribute("aria-expanded")).toBe("false");
    expect(group.contains(testId("sidebar-context-menu-switch-agent"))).toBe(true);
    expect(group.hidden).toBe(true);
    click("sidebar-context-menu-more");
    expect(more.getAttribute("aria-expanded")).toBe("true");
    expect(group.hidden).toBe(false);
  });

  // The row menu is a modal sheet only at phone width.
  const atViewport = (phone: boolean) =>
    vi.stubGlobal(
      "matchMedia",
      vi.fn((query: string) => ({ matches: phone && query.includes("max-width"), media: query })),
    );

  it("on a desktop viewport stays a plain menu: not modal, no focus takeover, Tab not trapped", () => {
    atViewport(false);
    const menu = openRowMenu(ws({ view: "structured" }), { expandMore: false });
    expect(menu.getAttribute("role")).toBeNull();
    expect(menu.getAttribute("aria-modal")).toBeNull();
    expect(menu.contains(document.activeElement)).toBe(false);
    const items = [...menu.querySelectorAll<HTMLElement>("button")].filter((el) => !el.closest("[hidden]"));
    const last = items[items.length - 1]!;
    last.focus();
    fireEvent.keyDown(last, { key: "Tab" });
    expect(document.activeElement).toBe(last);
    // Escape still closes it.
    fireEvent.keyDown(last, { key: "Escape" });
    expect(testId("sidebar-context-menu")).toBeNull();
  });

  it("closes when the viewport crosses the phone breakpoint while open", () => {
    const listeners: (() => void)[] = [];
    vi.stubGlobal(
      "matchMedia",
      vi.fn((query: string) => ({
        matches: false,
        media: query,
        addEventListener: (_: string, fn: () => void) => listeners.push(fn),
        removeEventListener: vi.fn(),
      })),
    );
    openRowMenu(ws(), { expandMore: false });
    expect(listeners.length).toBeGreaterThan(0);
    act(() => listeners.forEach((fn) => fn()));
    expect(testId("sidebar-context-menu")).toBeNull();
  });

  it("keeps Tab and Shift+Tab inside the sheet, skipping folded actions", () => {
    atViewport(true);
    const menu = openRowMenu(ws({ view: "structured" }), { expandMore: false });
    const items = [...menu.querySelectorAll<HTMLElement>("button")].filter((el) => !el.closest("[hidden]"));
    const [first, last] = [items[0]!, items[items.length - 1]!];
    last.focus();
    fireEvent.keyDown(last, { key: "Tab" });
    expect(document.activeElement).toBe(first);
    fireEvent.keyDown(first, { key: "Tab", shiftKey: true });
    expect(document.activeElement).toBe(last);
  });

  it("is a named modal sheet that takes focus, closes on Escape, and returns focus to the row", () => {
    atViewport(true);
    const menu = openRowMenu(ws({ title: "Fix login" }), { expandMore: false });
    expect(menu.getAttribute("role")).toBe("dialog");
    expect(menu.getAttribute("aria-modal")).toBe("true");
    expect(menu.getAttribute("aria-label")).toBe("Fix login actions");
    expect(menu.contains(document.activeElement)).toBe(true);
    fireEvent.keyDown(document.activeElement!, { key: "Escape" });
    expect(testId("sidebar-context-menu")).toBeNull();
    expect(document.activeElement).toBe(screen.getByTestId("sidebar-session-row"));
  });

  it.each([
    // Archiving or snoozing a pinned session clears the pin server-side, as in the TUI.
    ["pinned", { pinned_at: PAST }, ["Pinned", "Archive", "Snooze"], []],
    ["archived", { archived_at: PAST }, ["Unarchive"], ["Pin", "Snooze"]],
    ["snoozed", { snoozed_until: inMinutes(60) }, ["Unsnooze"], ["Pin", "Archive"]],
    ["live", {}, ["Pin", "Archive", "Snooze"], []],
  ] as [string, Partial<SessionResponse>, string[], string[]][])("%s row triage items", (_n, over, has, lacks) => {
    const text = openRowMenu(ws(over)).textContent;
    for (const t of has) expect(text).toContain(t);
    for (const t of lacks) expect(text).not.toContain(t);
  });

  it("offers Switch agent only on structured rows", () => {
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
  it.each([
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
  ] as [string, Partial<SessionResponse>, string, string, unknown][])(
    "%s PATCHes its endpoint",
    async (_n, over, item, path, body) => {
      openRowMenu(ws({ id: "sess-it", ...over }));
      click(item);
      await vi.waitFor(() => expect(fetchSpy).toHaveBeenCalled());
      expect(firstRequest(fetchSpy)).toEqual({ url: `/api/sessions/sess-it/${path}`, method: "PATCH", body });
    },
  );

  it.each([
    ["Pin", "sidebar-context-menu-pin", "Pinned"],
    ["Archive", "sidebar-context-menu-archive", "Archived"],
  ])("%s shows its chip optimistically and reverts it on failure", async (_n, item, chip) => {
    let fail = () => {};
    fetchSpy.mockImplementation(
      () => new Promise((resolve) => (fail = () => resolve(new Response("nope", { status: 500 })))),
    );
    openRowMenu(ws({ id: "sess-fail" }));
    click(item);
    // Regression: the chip must read the optimistic state, not wait for the poll.
    await vi.waitFor(() => expect(label(chip)).not.toBeNull());
    // Pin toggles in place; Archive moves the row out of view, so it closes the menu.
    expect(testId("sidebar-context-menu") != null).toBe(item === "sidebar-context-menu-pin");
    fail();
    await vi.waitFor(() => expect(label(chip)).toBeNull());
  });

  it.each([
    ["Notify all", "sidebar-context-menu-notify-all", "notifications"],
    ["Color", "sidebar-context-menu-color-red", "color"],
  ])("%s keeps the menu open, shows the pick, and reverts on failure", async (_n, item, path) => {
    let fail = () => {};
    fetchSpy.mockImplementation(
      () => new Promise((resolve) => (fail = () => resolve(new Response("nope", { status: 500 })))),
    );
    openRowMenu(ws({ id: "sess-pick" }));
    click(item);
    expect(testId("sidebar-context-menu")).not.toBeNull();
    expect(testId(item)!.getAttribute("aria-pressed")).toBe("true");
    expect(fetchSpy.mock.calls[0]![0]).toBe(`/api/sessions/sess-pick/${path}`);
    fail();
    await vi.waitFor(() => expect(testId(item)!.getAttribute("aria-pressed")).toBe("false"));
    expect(testId("sidebar-context-menu")).not.toBeNull();
  });

  it("the close button closes the menu", () => {
    openRowMenu(ws());
    click("sidebar-context-menu-close");
    expect(testId("sidebar-context-menu")).toBeNull();
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
  it.each([
    ["red", {}, "red"],
    ["unset", {}, null],
    ["unknown", {}, null],
    // Disabling colors hides the stored value without clearing it (#3104).
    ["red", { colorsEnabled: false }, null],
  ] as const)("color %s with %o shows dot %s", (color, options, shown) => {
    renderRow(ws({ color: color === "unset" ? undefined : color === "unknown" ? "chartreuse" : color }), options);
    const dot = testId("sidebar-session-color-dot");
    expect(dot?.getAttribute("data-color") ?? null).toBe(shown);
    if (shown) expect(dot!.className).toContain("bg-red-500");
  });

  it("marks No color as picked on an uncolored row and hides the section when disabled", () => {
    openRowMenu(ws());
    expect(testId("sidebar-context-menu-color-clear")!.getAttribute("aria-pressed")).toBe("true");
    expect(testId("sidebar-context-menu-color-red")!.getAttribute("aria-pressed")).toBe("false");
    cleanup();
    openRowMenu(ws({ color: "green" }), { colorsEnabled: false });
    for (const key of ["red", "amber", "green", "clear"]) {
      expect(testId(`sidebar-context-menu-color-${key}`)).toBeNull();
    }
  });
});
