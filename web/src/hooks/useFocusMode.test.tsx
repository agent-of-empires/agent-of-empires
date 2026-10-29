// @vitest-environment jsdom

import { describe, expect, it } from "vitest";
import { act, renderHook } from "@testing-library/react";
import { MemoryRouter, useLocation, useMatch, useNavigate } from "react-router-dom";
import type { ReactNode } from "react";

import { useFocusMode } from "./useFocusMode";

function setup(entries: string[]) {
  const wrapper = ({ children }: { children: ReactNode }) => (
    <MemoryRouter initialEntries={entries} initialIndex={entries.length - 1}>
      {children}
    </MemoryRouter>
  );
  return renderHook(
    () => {
      const location = useLocation();
      const navigate = useNavigate();
      const routed = useMatch("/session/:sessionId")?.params.sessionId ?? null;
      return { focus: useFocusMode(routed), url: location.pathname + location.search, navigate };
    },
    { wrapper },
  );
}

describe("useFocusMode", () => {
  it.each([
    ["/session/s1?focus=1", "s1"],
    ["/session/a%20b?focus=1", "a b"],
    ["/session/s1", null],
    ["/session/s1?focus=0", null],
    // `focus=1` only scopes a session route.
    ["/settings?focus=1", null],
  ])("reads %s as focus session %s", (url, expected) => {
    expect(setup([url]).result.current.focus.focusSessionId).toBe(expected);
  });

  it("enters and exits in place, so Back leaves the session instead of toggling the mode", () => {
    const { result } = setup(["/", "/session/s1"]);
    act(() => result.current.focus.enterFocusMode("s1"));
    expect(result.current.url).toBe("/session/s1?focus=1");
    expect(result.current.focus.focusSessionId).toBe("s1");

    act(() => result.current.focus.exitFocusMode());
    expect(result.current.url).toBe("/session/s1");
    expect(result.current.focus.focusSessionId).toBeNull();

    act(() => result.current.navigate(-1));
    expect(result.current.url).toBe("/");
  });

  it("does nothing on exit outside focus mode", () => {
    const { result } = setup(["/session/s1"]);
    act(() => result.current.focus.exitFocusMode());
    expect(result.current.url).toBe("/session/s1");
  });
});
