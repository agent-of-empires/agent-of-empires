// @vitest-environment jsdom

import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { renderHook } from "@testing-library/react";
import { MemoryRouter, useMatch } from "react-router-dom";
import type { ReactNode } from "react";

import { useFocusMode, useFocusSession } from "./useFocusMode";

const originalLocation = Object.getOwnPropertyDescriptor(window, "location")!;
let replace: ReturnType<typeof vi.fn>;

beforeEach(() => {
  replace = vi.fn();
  Object.defineProperty(window, "location", { configurable: true, value: { ...window.location, replace } });
});
afterEach(() => {
  Object.defineProperty(window, "location", originalLocation);
});

function setup(url: string) {
  const wrapper = ({ children }: { children: ReactNode }) => (
    <MemoryRouter initialEntries={[url]}>{children}</MemoryRouter>
  );
  return renderHook(() => useFocusMode(useMatch("/session/:sessionId")?.params.sessionId ?? null), { wrapper });
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
    expect(setup(url).result.current.focusSessionId).toBe(expected);
  });

  // A router navigation would leave the service worker's `Client.url` on the old mode.
  it("enters and exits by loading a new document in place of this history entry", () => {
    setup("/session/a%20b").result.current.enterFocusMode("a b");
    expect(replace).toHaveBeenLastCalledWith("/session/a%20b?focus=1");

    setup("/session/a%20b?focus=1").result.current.exitFocusMode();
    expect(replace).toHaveBeenLastCalledWith("/session/a%20b");
    expect(replace).toHaveBeenCalledTimes(2);
  });

  it("does nothing on exit outside focus mode", () => {
    setup("/session/s1").result.current.exitFocusMode();
    expect(replace).not.toHaveBeenCalled();
  });
});

describe("useFocusSession", () => {
  type Session = { title: string; branch?: string | null; trashed_at?: string | null };
  const render = (focus: string | null, session: Session | undefined, loaded = true) =>
    renderHook(
      (p: { focus: string | null; session: Session | undefined; loaded: boolean }) =>
        useFocusSession(p.focus, p.session, p.loaded),
      { initialProps: { focus, session, loaded } },
    );

  afterEach(() => {
    document.title = "Agent of Empires";
  });

  it("titles the tab after the focused session and restores it on leaving", () => {
    document.title = "Agent of Empires";
    const { result, rerender, unmount } = render("s1", { title: "Mongols" });
    expect(result.current).toEqual({ title: "Mongols", gone: null });
    expect(document.title).toBe("Mongols");

    rerender({ focus: "s1", session: { title: " ", branch: "feat/x" }, loaded: true });
    expect(document.title).toBe("feat/x");
    unmount();
    expect(document.title).toBe("Agent of Empires");
  });

  it("leaves a regular window's title alone", () => {
    document.title = "Agent of Empires";
    expect(render(null, { title: "Mongols" }).result.current).toEqual({ title: null, gone: null });
    expect(document.title).toBe("Agent of Empires");
  });

  it("reports a trashed session, and a deleted one under its last known name", () => {
    const { result, rerender } = render("s1", { title: "Mongols" });
    rerender({ focus: "s1", session: { title: "Mongols", trashed_at: "2026-01-01T00:00:00Z" }, loaded: true });
    expect(result.current).toEqual({ title: "Mongols", gone: "trashed" });

    rerender({ focus: "s1", session: undefined, loaded: true });
    expect(result.current).toEqual({ title: "Mongols", gone: "deleted" });
    expect(document.title).toBe("Mongols");
  });

  it("does not call a session gone before the first load, nor name one it never saw", () => {
    expect(render("s1", undefined, false).result.current).toEqual({ title: null, gone: null });
    expect(render("s1", undefined, true).result.current).toEqual({ title: null, gone: "deleted" });
  });
});
