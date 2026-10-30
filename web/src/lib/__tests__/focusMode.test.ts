import { describe, expect, it } from "vitest";

import { focusNavigation, focusSessionTitle, noticedSessions } from "../focusMode";

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
