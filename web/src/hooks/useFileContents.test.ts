// @vitest-environment jsdom

import { act, renderHook, waitFor } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import { useFileContents, __resetFileContentsCache } from "./useFileContents";
import * as api from "../lib/api";
import type { RichFileContentsResponse } from "../lib/types";

const contents = (path: string, body: string): RichFileContentsResponse => ({
  file: { path, old_path: null, status: "modified", additions: 1, deletions: 0 },
  old_content: "",
  new_content: body,
  patch: `@@ -0,0 +1 @@\n+${body}`,
  is_binary: false,
  truncated: false,
});

let spy: ReturnType<typeof vi.spyOn<typeof api, "getSessionFileContents">>;

/** Render on `path`, wait for `body`, and return helpers to switch files. */
async function mount(path: string | null, body?: string, rev?: number) {
  if (body !== undefined && path) spy.mockResolvedValueOnce(contents(path, body));
  const hook = renderHook(({ path, rev }) => useFileContents("s1", path, undefined, rev), {
    initialProps: { path, rev },
  });
  const shown = () => hook.result.current.contents?.new_content;
  if (body !== undefined) await waitFor(() => expect(shown()).toBe(body));
  const open = async (next: string, nextBody: string, nextRev = rev) => {
    spy.mockResolvedValueOnce(contents(next, nextBody));
    hook.rerender({ path: next, rev: nextRev });
    await waitFor(() => expect(shown()).toBe(nextBody));
  };
  return { ...hook, shown, open };
}

describe("useFileContents", () => {
  beforeEach(() => {
    __resetFileContentsCache();
    spy = vi.spyOn(api, "getSessionFileContents");
  });
  afterEach(() => vi.restoreAllMocks());

  it("serves a revisit from cache without re-fetching", async () => {
    const { result, rerender, open, shown } = await mount("a.ts", "alpha");
    await open("b.ts", "beta");
    rerender({ path: "a.ts", rev: undefined });
    expect(shown()).toBe("alpha");
    expect(result.current.loading).toBe(false);
    expect(spy).toHaveBeenCalledTimes(2);
  });

  it.each([
    ["a bumped revision", async (m: Awaited<ReturnType<typeof mount>>) => m.open("a.ts", "v2", 2)],
    [
      "an exceeded byte budget",
      async (m: Awaited<ReturnType<typeof mount>>) => {
        await m.open("big.ts", "x".repeat(33 * 1024 * 1024));
        await m.open("a.ts", "v2");
      },
    ],
    [
      "refresh()",
      async (m: Awaited<ReturnType<typeof mount>>) => {
        spy.mockResolvedValueOnce(contents("a.ts", "v2"));
        await act(async () => m.result.current.refresh());
        await waitFor(() => expect(m.shown()).toBe("v2"));
      },
    ],
  ])("re-fetches a cached file after %s", async (_label, invalidate) => {
    const m = await mount("a.ts", "v1", 1);
    await invalidate(m);
    expect(m.shown()).toBe("v2");
  });

  it("keeps stale contents under a loading flag while an uncached file loads", async () => {
    const { result, rerender, shown } = await mount("a.ts", "alpha");
    let resolve: (v: RichFileContentsResponse | null) => void = () => {};
    spy.mockImplementationOnce(() => new Promise((r) => (resolve = r)));
    rerender({ path: "b.ts", rev: undefined });
    expect(shown()).toBe("alpha");
    expect(result.current.loading).toBe(true);

    await waitFor(() => expect(spy).toHaveBeenCalledTimes(2));
    resolve(contents("b.ts", "beta"));
    await waitFor(() => expect(shown()).toBe("beta"));
    expect(result.current.loading).toBe(false);
  });

  it("surfaces an error when the fetch returns no contents", async () => {
    spy.mockResolvedValue(null);
    const { result } = await mount("a.ts");
    await waitFor(() => expect(result.current.error).toBe("Failed to load file contents"));
    expect(result.current).toMatchObject({ contents: null, loading: false });
  });

  it("does not fetch while filePath is null", async () => {
    const { result, open } = await mount(null);
    await act(() => new Promise((r) => setTimeout(r, 5)));
    expect(spy).not.toHaveBeenCalled();
    expect(result.current).toMatchObject({ contents: null, loading: false });
    await open("a.ts", "alpha");
    expect(spy).toHaveBeenCalledTimes(1);
  });

  it("drops a superseded in-flight response when switching files rapidly", async () => {
    let resolveA: (v: RichFileContentsResponse | null) => void = () => {};
    spy.mockImplementationOnce(() => new Promise((r) => (resolveA = r)));
    const { open, shown } = await mount("a.ts");
    await waitFor(() => expect(spy).toHaveBeenCalledTimes(1));
    await open("b.ts", "beta");
    await act(async () => {
      resolveA(contents("a.ts", "alpha-late"));
      await new Promise((r) => setTimeout(r, 5));
    });
    expect(shown()).toBe("beta");
  });
});
