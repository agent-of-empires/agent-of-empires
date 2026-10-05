// @vitest-environment jsdom

import { afterEach, describe, expect, it, vi } from "vitest";
import { act, cleanup, renderHook, waitFor } from "@testing-library/react";
import { useProjects } from "../useProjects";

afterEach(() => {
  cleanup();
  vi.clearAllTimers();
  vi.useRealTimers();
  vi.unstubAllGlobals();
});

describe("served project registry", () => {
  it("keeps machine-default projects out of a pinned daemon and refreshes a renamed identity", async () => {
    let served = "alpha";
    const registries = {
      alpha: [{ name: "alpha-project", path: "/alpha", scope: "profile", pinned: true }],
      renamed: [{ name: "renamed-project", path: "/renamed", scope: "profile", pinned: false }],
      main: [{ name: "wrong-project", path: "/main", scope: "profile", pinned: true }],
    };
    vi.stubGlobal(
      "fetch",
      vi.fn(
        async (url: string) =>
          new Response(
            JSON.stringify(
              url === "/api/about"
                ? { profile: served }
                : url === "/api/profiles"
                  ? [
                      { name: "main", is_default: true },
                      { name: served, is_default: false },
                    ]
                  : registries[new URL(url, "http://localhost").searchParams.get("profile") as keyof typeof registries],
            ),
            { status: 200 },
          ),
      ),
    );
    const { result } = renderHook(() => useProjects());
    await waitFor(() => expect(result.current.ready).toBe(true));
    expect(result.current.projects.map((p) => p.path)).toEqual(["/alpha"]);
    served = "renamed";
    await act(async () => {
      await result.current.refresh();
    });
    expect(result.current.profile).toBe("renamed");
    expect(result.current.projects.map((p) => p.path)).toEqual(["/renamed"]);
  });

  it("does not admit writes from a failed or superseded project refresh", async () => {
    let release!: (value: Response) => void;
    let served = "alpha";
    let fail = false;
    const old = new Promise<Response>((resolve) => {
      release = resolve;
    });
    vi.stubGlobal(
      "fetch",
      vi.fn(async (url: string) => {
        if (url === "/api/about")
          return new Response(JSON.stringify({ profile: served }), { status: fail ? 503 : 200 });
        if (new URL(url, "http://localhost").searchParams.get("profile") === "alpha") return old;
        return new Response(JSON.stringify([{ name: "current", path: "/current", scope: "profile", pinned: true }]), {
          status: 200,
        });
      }),
    );
    const { result } = renderHook(() => useProjects());
    await waitFor(() => expect(result.current.profile).toBe("alpha"));
    expect(result.current.ready).toBe(false);
    served = "renamed";
    await act(async () => {
      await result.current.refresh();
    });
    await act(async () => {
      release(
        new Response(JSON.stringify([{ name: "stale", path: "/stale", scope: "profile", pinned: true }]), {
          status: 200,
        }),
      );
    });
    expect(result.current.projects.map((p) => p.path)).toEqual(["/current"]);
    expect(result.current.profile).toBe("renamed");
    fail = true;
    await act(async () => {
      await result.current.refresh();
    });
    expect(result.current.ready).toBe(false);
    expect(result.current.profile).toBe("renamed");
  });
  it.each(["about", "projects"])(
    "aborts a stalled %s read at its deadline and on disposal, preserving the last registry but gating writes",
    async (stage) => {
      vi.useFakeTimers();
      const signals: AbortSignal[] = [];
      let stall = false;
      const fetchSpy = vi.fn(async (url: string, init?: RequestInit) => {
        if (stall && (stage === "about" ? url === "/api/about" : url.startsWith("/api/projects"))) {
          return new Promise<Response>((_resolve, reject) => {
            signals.push(init!.signal as AbortSignal);
            init!.signal!.addEventListener("abort", () => reject(new DOMException("aborted", "AbortError")), {
              once: true,
            });
          });
        }
        return new Response(
          JSON.stringify(
            url === "/api/about"
              ? { profile: "alpha" }
              : [{ name: "saved", path: "/alpha", scope: "profile", pinned: true }],
          ),
        );
      });
      vi.stubGlobal("fetch", fetchSpy);
      const { result, unmount } = renderHook(() => useProjects());
      await act(async () => {
        await vi.advanceTimersByTimeAsync(0);
      });
      expect(result.current.ready).toBe(true);
      stall = true;
      let refresh!: Promise<void>;
      await act(async () => {
        refresh = result.current.refresh();
        await vi.advanceTimersByTimeAsync(0);
      });
      expect(result.current.ready).toBe(false);
      await act(async () => {
        await vi.advanceTimersByTimeAsync(15000);
        await refresh;
      });
      expect(signals[0]!.aborted).toBe(true);
      expect(result.current.projects.map((project) => project.path)).toEqual(["/alpha"]);
      expect(result.current.ready).toBe(false);
      stall = false;
      await act(async () => {
        await result.current.refresh();
      });
      expect(result.current.ready).toBe(true);
      stall = true;
      await act(async () => {
        void result.current.refresh();
        await vi.advanceTimersByTimeAsync(0);
      });
      unmount();
      expect(signals[1]!.aborted).toBe(true);
      const calls = fetchSpy.mock.calls.length;
      await act(async () => {
        await vi.advanceTimersByTimeAsync(30000);
      });
      expect(fetchSpy).toHaveBeenCalledTimes(calls);
    },
  );

  it("aborts superseded reads and rejects their delivery even if the transport ignores abort", async () => {
    const late = Promise.withResolvers<Response>();
    let served = "alpha";
    let oldSignal!: AbortSignal;
    vi.stubGlobal(
      "fetch",
      vi.fn(async (url: string, init?: RequestInit) => {
        if (url === "/api/about") return new Response(JSON.stringify({ profile: served }));
        if (new URL(url, "http://localhost").searchParams.get("profile") === "alpha") {
          oldSignal = init!.signal as AbortSignal;
          return late.promise;
        }
        return new Response(JSON.stringify([{ name: "new", path: "/beta", scope: "profile", pinned: true }]));
      }),
    );
    const { result } = renderHook(() => useProjects());
    await waitFor(() => expect(result.current.profile).toBe("alpha"));
    served = "beta";
    await act(async () => {
      await result.current.refresh();
    });
    expect(oldSignal.aborted).toBe(true);
    await act(async () =>
      late.resolve(new Response(JSON.stringify([{ name: "old", path: "/old", scope: "profile" }]))),
    );
    expect(result.current.profile).toBe("beta");
    expect(result.current.projects.map((project) => project.path)).toEqual(["/beta"]);
    expect(result.current.ready).toBe(true);
  });
});
