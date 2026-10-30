// @vitest-environment jsdom

import { afterEach, describe, expect, it, vi } from "vitest";

import { focusSessionPath, openFocusWindow } from "../sessionRoute";

describe("focus window routes", () => {
  afterEach(() => vi.restoreAllMocks());

  it("opens a session's focus window beside this one, with its id escaped", () => {
    const open = vi.spyOn(window, "open").mockReturnValue(null);
    openFocusWindow("a b/c");
    expect(open).toHaveBeenCalledWith("/session/a%20b%2Fc?focus=1", "_blank");
    expect(focusSessionPath("a b/c")).toBe("/session/a%20b%2Fc?focus=1");
  });
});
