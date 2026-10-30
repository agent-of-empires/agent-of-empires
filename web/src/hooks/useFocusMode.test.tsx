// @vitest-environment jsdom

import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { act, renderHook } from "@testing-library/react";
import { MemoryRouter, useMatch, useNavigate } from "react-router-dom";
import type { ReactNode } from "react";

import { useFocusMode, useFocusSession } from "./useFocusMode";
import { focusSessionOfUrl } from "../lib/focusMode";
import { loadedFocusSessionId } from "../lib/sessionRoute";

vi.mock("../lib/sessionRoute", async (importOriginal) => ({
  ...(await importOriginal<typeof import("../lib/sessionRoute")>()),
  loadedFocusSessionId: vi.fn(),
}));

const originalLocation = Object.getOwnPropertyDescriptor(window, "location")!;
let replace: ReturnType<typeof vi.fn>;

beforeEach(() => {
  replace = vi.fn();
  Object.defineProperty(window, "location", { configurable: true, value: { ...window.location, replace } });
});
afterEach(() => {
  Object.defineProperty(window, "location", originalLocation);
});

/** Renders the hook on `url`, as a document that loaded at `url` unless `loadedAt` says otherwise. */
function setup(url: string, loadedAt = url) {
  vi.mocked(loadedFocusSessionId).mockReturnValue(focusSessionOfUrl(loadedAt));
  const wrapper = ({ children }: { children: ReactNode }) => (
    <MemoryRouter initialEntries={[url]}>{children}</MemoryRouter>
  );
  return renderHook(
    () => ({
      focus: useFocusMode(useMatch("/session/:sessionId")?.params.sessionId ?? null),
      navigate: useNavigate(),
    }),
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
    expect(setup(url).result.current.focus.focusSessionId).toBe(expected);
    expect(replace).not.toHaveBeenCalled();
  });

  // A router navigation would leave the service worker's `Client.url` on the old mode.
  it("enters and exits by loading a new document in place of this history entry", () => {
    setup("/session/a%20b").result.current.focus.enterFocusMode("a b");
    expect(replace).toHaveBeenLastCalledWith("/session/a%20b?focus=1");

    setup("/session/a%20b?focus=1").result.current.focus.exitFocusMode();
    expect(replace).toHaveBeenLastCalledWith("/session/a%20b");
    expect(replace).toHaveBeenCalledTimes(2);
  });

  it("does nothing on exit outside focus mode", () => {
    setup("/session/s1").result.current.focus.exitFocusMode();
    expect(replace).not.toHaveBeenCalled();
  });

  it("reloads when a router navigation changes the mode, so the service worker sees it", () => {
    const { result } = setup("/session/s1");
    act(() => result.current.navigate("/session/s1?focus=1#turn-3"));
    expect(replace).toHaveBeenLastCalledWith("/session/s1?focus=1#turn-3");

    // Back into a regular entry from a document loaded as a focus window.
    const back = setup("/session/s1?focus=1");
    act(() => back.result.current.navigate("/session/s1"));
    expect(replace).toHaveBeenLastCalledWith("/session/s1");

    // A session change within the same mode stays a router navigation.
    replace.mockClear();
    const regular = setup("/session/s1");
    act(() => regular.result.current.navigate("/session/s2"));
    expect(replace).not.toHaveBeenCalled();
  });

  it("reloads a document whose load URL and route disagree from the start", () => {
    setup("/session/s1?focus=1", "/session/s1");
    expect(replace).toHaveBeenCalledWith("/session/s1?focus=1");
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
