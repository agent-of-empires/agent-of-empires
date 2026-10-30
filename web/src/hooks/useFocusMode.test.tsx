// @vitest-environment jsdom

import { afterEach, describe, expect, it, vi } from "vitest";
import { renderHook } from "@testing-library/react";
import { MemoryRouter, useMatch } from "react-router-dom";
import type { ReactNode } from "react";

import { useFocusMode } from "./useFocusMode";
import { replaceDocument } from "../lib/sessionRoute";

vi.mock("../lib/sessionRoute", async (importOriginal) => ({
  ...(await importOriginal<typeof import("../lib/sessionRoute")>()),
  replaceDocument: vi.fn(),
}));

function setup(url: string) {
  const wrapper = ({ children }: { children: ReactNode }) => (
    <MemoryRouter initialEntries={[url]}>{children}</MemoryRouter>
  );
  return renderHook(() => useFocusMode(useMatch("/session/:sessionId")?.params.sessionId ?? null), { wrapper });
}

describe("useFocusMode", () => {
  afterEach(() => vi.mocked(replaceDocument).mockClear());

  it.each([
    ["/session/s1?focus=1", "s1"],
    ["/session/a%20b?focus=1", "a b"],
    ["/session/s1", null],
    ["/session/s1?focus=0", null],
    // `focus=1` only scopes a session route.
    ["/settings?focus=1", null],
  ])("reads %s as focus session %s", (url, expected) => {
    expect(setup(url).result.current.focusSessionId).toBe(expected);
  });

  // A router navigation would leave the service worker's `Client.url` on the old mode.
  it("enters and exits by loading a new document in place of this history entry", () => {
    setup("/session/a%20b").result.current.enterFocusMode("a b");
    expect(replaceDocument).toHaveBeenLastCalledWith("/session/a%20b?focus=1");

    setup("/session/a%20b?focus=1").result.current.exitFocusMode();
    expect(replaceDocument).toHaveBeenLastCalledWith("/session/a%20b");
    expect(replaceDocument).toHaveBeenCalledTimes(2);
  });

  it("does nothing on exit outside focus mode", () => {
    setup("/session/s1").result.current.exitFocusMode();
    expect(replaceDocument).not.toHaveBeenCalled();
  });
});
