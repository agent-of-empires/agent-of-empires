// @vitest-environment jsdom

import { afterEach, describe, expect, it } from "vitest";
import { act, renderHook, waitFor } from "@testing-library/react";
import { MemoryRouter, useLocation, useNavigate } from "react-router-dom";
import type { ReactNode } from "react";

import { LAST_SESSION_KEY, useLastSessionRestore } from "./useLastSessionRestore";

type Params = {
  activeSessionId: string | null;
  sessions: readonly { id: string }[];
  sessionsLoaded: boolean;
};

function setup(initialEntry: string, initialProps: Params) {
  const wrapper = ({ children }: { children: ReactNode }) => (
    <MemoryRouter initialEntries={[initialEntry]}>{children}</MemoryRouter>
  );
  return renderHook(
    (props: Params) => {
      const location = useLocation();
      const navigate = useNavigate();
      useLastSessionRestore(props);
      return { location, navigate };
    },
    { wrapper, initialProps },
  );
}

afterEach(() => {
  localStorage.clear();
});

describe("useLastSessionRestore", () => {
  it("persists the active session id", async () => {
    setup("/session/s1", { activeSessionId: "s1", sessions: [{ id: "s1" }], sessionsLoaded: true });
    await waitFor(() => expect(localStorage.getItem(LAST_SESSION_KEY)).toBe("s1"));
  });

  it("restores the stored session on a cold launch to the dashboard root", async () => {
    localStorage.setItem(LAST_SESSION_KEY, "s1");
    const { result } = setup("/", {
      activeSessionId: null,
      sessions: [{ id: "s1" }],
      sessionsLoaded: true,
    });
    await waitFor(() => expect(result.current.location.pathname).toBe("/session/s1"));
  });

  it("drops a stored id that no longer matches a loaded session", async () => {
    localStorage.setItem(LAST_SESSION_KEY, "gone");
    const { result } = setup("/", {
      activeSessionId: null,
      sessions: [{ id: "s1" }],
      sessionsLoaded: true,
    });
    await waitFor(() => expect(localStorage.getItem(LAST_SESSION_KEY)).toBeNull());
    expect(result.current.location.pathname).toBe("/");
  });

  it("does nothing on a cold launch with no stored session", async () => {
    const { result } = setup("/", {
      activeSessionId: null,
      sessions: [{ id: "s1" }],
      sessionsLoaded: true,
    });
    // Give effects a tick; the dashboard stays put and no key is written.
    await waitFor(() => expect(result.current.location.pathname).toBe("/"));
    expect(localStorage.getItem(LAST_SESSION_KEY)).toBeNull();
  });

  it("waits for the sessions list before restoring", async () => {
    localStorage.setItem(LAST_SESSION_KEY, "s1");
    const { result, rerender } = setup("/", {
      activeSessionId: null,
      sessions: [],
      sessionsLoaded: false,
    });
    // Not loaded yet: no redirect.
    expect(result.current.location.pathname).toBe("/");
    rerender({ activeSessionId: null, sessions: [{ id: "s1" }], sessionsLoaded: true });
    await waitFor(() => expect(result.current.location.pathname).toBe("/session/s1"));
  });

  it("does not override a deep link to a session", async () => {
    localStorage.setItem(LAST_SESSION_KEY, "s1");
    const { result } = setup("/session/other", {
      activeSessionId: "other",
      sessions: [{ id: "s1" }, { id: "other" }],
      sessionsLoaded: true,
    });
    await waitFor(() => expect(localStorage.getItem(LAST_SESSION_KEY)).toBe("other"));
    expect(result.current.location.pathname).toBe("/session/other");
  });

  it("clears the stored session on an in-app return to the dashboard", async () => {
    const { result, rerender } = setup("/session/s1", {
      activeSessionId: "s1",
      sessions: [{ id: "s1" }],
      sessionsLoaded: true,
    });
    await waitFor(() => expect(localStorage.getItem(LAST_SESSION_KEY)).toBe("s1"));

    act(() => result.current.navigate("/"));
    rerender({ activeSessionId: null, sessions: [{ id: "s1" }], sessionsLoaded: true });

    await waitFor(() => expect(localStorage.getItem(LAST_SESSION_KEY)).toBeNull());
    expect(result.current.location.pathname).toBe("/");
  });
});
