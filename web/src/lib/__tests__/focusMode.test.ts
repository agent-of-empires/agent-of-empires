import { describe, expect, it } from "vitest";

import { createToasts, focusNavigation, focusSessionOfUrl, focusSessionTitle, noticedSessions } from "../focusMode";
import { toastBus, type ToastApi } from "../toastBus";

describe("createToasts", () => {
  it("reports background creates in a regular window only", () => {
    const api = { info: () => {} } as unknown as ToastApi;
    toastBus.handler = api;
    try {
      expect(createToasts(false)).toBe(api);
      expect(createToasts(true)).toBeNull();
    } finally {
      toastBus.handler = null;
    }
  });
});

describe("focusSessionOfUrl", () => {
  it.each([
    ["https://aoe.test/session/s1?focus=1", "s1"],
    ["https://aoe.test/session/a%20b/?focus=1#turn", "a b"],
    ["/session/s1?focus=1", "s1"],
    ["https://aoe.test/session/s1", null],
    ["https://aoe.test/session/s1?focus=0", null],
    ["https://aoe.test/session/s1/diff?focus=1", null],
    ["https://aoe.test/settings?focus=1", null],
  ])("reads %s as %s", (url, expected) => {
    expect(focusSessionOfUrl(url)).toBe(expected);
  });
});

describe("focusSessionTitle", () => {
  it.each([
    [{ title: "Mongols", branch: "feat/x" }, "Mongols"],
    [{ title: "  ", branch: "feat/x" }, "feat/x"],
    [{ title: "", branch: null }, "Untitled session"],
  ])("names %j as %s", (session, expected) => {
    expect(focusSessionTitle(session)).toBe(expected);
  });
});

describe("noticedSessions", () => {
  const sessions = [{ id: "a" }, { id: "b" }];

  it.each([
    [null, ["a", "b"]],
    ["b", ["b"]],
    // A deleted focus session leaves nothing to report rather than falling back to all.
    ["gone", []],
  ])("with focus %s reports %j", (focus, ids) => {
    expect(noticedSessions(sessions, focus).map((s) => s.id)).toEqual(ids);
  });
});

describe("focusNavigation", () => {
  it.each([
    [null, "b", "here"],
    [null, undefined, "here"],
    ["a", "a", "none"],
    ["a", "b", "new-window"],
    ["a", undefined, "new-window"],
  ] as const)("in focus %s, a target session %s goes %s", (focus, target, expected) => {
    expect(focusNavigation(focus, target)).toBe(expected);
  });
});
