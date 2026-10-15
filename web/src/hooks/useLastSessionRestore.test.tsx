// @vitest-environment jsdom

import { afterEach, describe, expect, it, vi } from "vitest";
import { act, renderHook, waitFor } from "@testing-library/react";
import { MemoryRouter, useLocation, useNavigate } from "react-router-dom";
import type { ReactNode } from "react";

import { LAST_SESSION_KEY, useLastSessionRestore } from "./useLastSessionRestore";

type Params = {
  activeSessionId: string | null;
  sessions: readonly { id: string }[];
  sessionsLoaded: boolean;
};

const params = (over: Partial<Params> = {}): Params => ({
  activeSessionId: null,
  sessions: [{ id: "s1" }],
  sessionsLoaded: true,
  ...over,
});

function setup(initialEntry: string, initialProps: Params = params()) {
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

function stubStandalone(standalone: boolean) {
  vi.stubGlobal(
    "matchMedia",
    vi.fn((query: string) => ({ matches: query === "(display-mode: standalone)" && standalone, media: query })),
  );
}

afterEach(() => {
  localStorage.clear();
  vi.restoreAllMocks();
  vi.unstubAllGlobals();
});

describe("useLastSessionRestore", () => {
  // Only a standalone PWA cold launch resumes; a browser tab stays on the dashboard.
  it.each([
    [true, "/session/s1"],
    [false, "/"],
  ])("standalone=%s restores a stored session to %s", async (standalone, pathname) => {
    stubStandalone(standalone);
    localStorage.setItem(LAST_SESSION_KEY, "s1");
    const { result } = setup("/");
    await waitFor(() => expect(result.current.location.pathname).toBe(pathname));
  });

  it("drops a stored id that no longer matches a loaded session", async () => {
    stubStandalone(true);
    localStorage.setItem(LAST_SESSION_KEY, "gone");
    const { result } = setup("/");
    await waitFor(() => expect(localStorage.getItem(LAST_SESSION_KEY)).toBeNull());
    expect(result.current.location.pathname).toBe("/");
  });

  it("does nothing on a cold launch with no stored session", async () => {
    const { result } = setup("/");
    await waitFor(() => expect(result.current.location.pathname).toBe("/"));
    expect(localStorage.getItem(LAST_SESSION_KEY)).toBeNull();
  });

  it("waits for the sessions list before restoring", async () => {
    stubStandalone(true);
    localStorage.setItem(LAST_SESSION_KEY, "s1");
    const { result, rerender } = setup("/", params({ sessions: [], sessionsLoaded: false }));
    expect(result.current.location.pathname).toBe("/");
    rerender(params());
    await waitFor(() => expect(result.current.location.pathname).toBe("/session/s1"));
  });

  it("does not override a deep link to a session", async () => {
    localStorage.setItem(LAST_SESSION_KEY, "s1");
    const { result } = setup(
      "/session/other",
      params({ activeSessionId: "other", sessions: [{ id: "s1" }, { id: "other" }] }),
    );
    await waitFor(() => expect(localStorage.getItem(LAST_SESSION_KEY)).toBe("other"));
    expect(result.current.location.pathname).toBe("/session/other");
  });

  it("clears the stored session on an in-app return to the dashboard", async () => {
    const { result, rerender } = setup("/session/s1", params({ activeSessionId: "s1" }));
    await waitFor(() => expect(localStorage.getItem(LAST_SESSION_KEY)).toBe("s1"));

    act(() => result.current.navigate("/"));
    rerender(params());

    await waitFor(() => expect(localStorage.getItem(LAST_SESSION_KEY)).toBeNull());
    expect(result.current.location.pathname).toBe("/");
  });
});
