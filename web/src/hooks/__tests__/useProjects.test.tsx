// @vitest-environment jsdom

import { afterEach, describe, expect, it, vi } from "vitest";
import { act, renderHook, waitFor } from "@testing-library/react";
import { useProjects } from "../useProjects";

afterEach(() => vi.unstubAllGlobals());

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
});
