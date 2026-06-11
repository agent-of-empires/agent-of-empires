// @vitest-environment jsdom
//
// Covers the client-side contents cache and switch behavior added to address
// diff-switch lag (#1969 follow-up): cache hits resolve without a loading flip
// or a re-fetch, a miss shows loading, a bumped revision invalidates, and the
// byte-budget LRU evicts the oldest entry once the budget is exceeded.

import { act, renderHook, waitFor } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import { useFileContents, __resetFileContentsCache } from "./useFileContents";
import * as api from "../lib/api";
import type { RichFileContentsResponse } from "../lib/types";

function makeContents(
  path: string,
  body: string,
  status: RichFileContentsResponse["file"]["status"] = "modified",
): RichFileContentsResponse {
  return {
    file: { path, old_path: null, status, additions: 1, deletions: 0 },
    old_content: "",
    new_content: body,
    patch: `@@ -0,0 +1 @@\n+${body}`,
    is_binary: false,
    truncated: false,
  };
}

describe("useFileContents", () => {
  beforeEach(() => {
    __resetFileContentsCache();
  });
  afterEach(() => {
    vi.restoreAllMocks();
  });

  it("fetches on first open, then serves a revisit from cache without re-fetching", async () => {
    const spy = vi.spyOn(api, "getSessionFileContents").mockResolvedValue(makeContents("a.ts", "alpha"));

    const { result, rerender } = renderHook(({ path }) => useFileContents("s1", path, undefined), {
      initialProps: { path: "a.ts" },
    });

    await waitFor(() => expect(result.current.contents).not.toBeNull());
    expect(result.current.contents?.new_content).toBe("alpha");
    expect(spy).toHaveBeenCalledTimes(1);

    // Switch away, then back to a.ts: the revisit is a cache hit.
    spy.mockResolvedValueOnce(makeContents("b.ts", "beta"));
    rerender({ path: "b.ts" });
    await waitFor(() => expect(result.current.contents?.new_content).toBe("beta"));
    expect(spy).toHaveBeenCalledTimes(2);

    rerender({ path: "a.ts" });
    // Cache hit: contents are already correct and no third fetch is made.
    expect(result.current.contents?.new_content).toBe("alpha");
    expect(result.current.loading).toBe(false);
    expect(spy).toHaveBeenCalledTimes(2);
  });

  it("keeps stale contents under a loading flag while switching to an uncached file", async () => {
    let resolve: (v: RichFileContentsResponse | null) => void = () => {};
    const spy = vi.spyOn(api, "getSessionFileContents");
    spy.mockResolvedValueOnce(makeContents("a.ts", "alpha"));

    const { result, rerender } = renderHook(({ path }) => useFileContents("s1", path, undefined), {
      initialProps: { path: "a.ts" },
    });
    await waitFor(() => expect(result.current.contents?.new_content).toBe("alpha"));

    // Switch to an uncached file whose fetch is still in flight.
    spy.mockImplementationOnce(() => new Promise((r) => (resolve = r)));
    rerender({ path: "b.ts" });
    // The previous file's contents stay painted (the viewer scrims them); the
    // loading flag is set so the scrim shows. No blank flash.
    expect(result.current.contents?.new_content).toBe("alpha");
    expect(result.current.loading).toBe(true);

    // The fetch is deferred a tick; wait until it's actually issued, then
    // resolve it.
    await waitFor(() => expect(spy).toHaveBeenCalledTimes(2));
    resolve(makeContents("b.ts", "beta"));
    await waitFor(() => expect(result.current.contents?.new_content).toBe("beta"));
    expect(result.current.loading).toBe(false);
  });

  it("invalidates the cache when the revision is bumped", async () => {
    const spy = vi.spyOn(api, "getSessionFileContents");
    spy.mockResolvedValueOnce(makeContents("a.ts", "v1"));

    const { result, rerender } = renderHook(({ rev }) => useFileContents("s1", "a.ts", undefined, rev), {
      initialProps: { rev: 1 },
    });
    await waitFor(() => expect(result.current.contents?.new_content).toBe("v1"));
    expect(spy).toHaveBeenCalledTimes(1);

    // Bumped revision = different cache key => re-fetch fresh contents.
    spy.mockResolvedValueOnce(makeContents("a.ts", "v2"));
    rerender({ rev: 2 });
    await waitFor(() => expect(result.current.contents?.new_content).toBe("v2"));
    expect(spy).toHaveBeenCalledTimes(2);
  });

  it("evicts the oldest entry once the byte budget is exceeded", async () => {
    // One payload larger than the 32MB budget forces eviction of any prior
    // entry on the next insert.
    const big = "x".repeat(33 * 1024 * 1024);
    const spy = vi.spyOn(api, "getSessionFileContents");
    spy.mockResolvedValueOnce(makeContents("a.ts", "alpha"));

    const { result, rerender } = renderHook(({ path }) => useFileContents("s1", path, undefined), {
      initialProps: { path: "a.ts" },
    });
    await waitFor(() => expect(result.current.contents?.new_content).toBe("alpha"));

    spy.mockResolvedValueOnce(makeContents("big.ts", big));
    rerender({ path: "big.ts" });
    await waitFor(() => expect(result.current.contents?.new_content).toBe(big));

    // a.ts was evicted by the oversized big.ts insert: revisiting re-fetches.
    spy.mockResolvedValueOnce(makeContents("a.ts", "alpha2"));
    rerender({ path: "a.ts" });
    await waitFor(() => expect(result.current.contents?.new_content).toBe("alpha2"));
    expect(spy).toHaveBeenCalledTimes(3);
  });

  it("refresh() forces a re-fetch even when the file is cached", async () => {
    const spy = vi.spyOn(api, "getSessionFileContents").mockResolvedValue(makeContents("a.ts", "alpha"));

    const { result } = renderHook(() => useFileContents("s1", "a.ts", undefined));
    await waitFor(() => expect(result.current.contents?.new_content).toBe("alpha"));
    expect(spy).toHaveBeenCalledTimes(1);

    // a.ts is cached, but refresh(force) bypasses the cache and re-fetches.
    spy.mockResolvedValueOnce(makeContents("a.ts", "alpha-refreshed"));
    await act(async () => {
      result.current.refresh();
    });
    await waitFor(() => expect(result.current.contents?.new_content).toBe("alpha-refreshed"));
    expect(spy).toHaveBeenCalledTimes(2);
  });

  it("surfaces an error when the fetch returns no contents", async () => {
    vi.spyOn(api, "getSessionFileContents").mockResolvedValue(null);

    const { result } = renderHook(() => useFileContents("s1", "a.ts", undefined));
    await waitFor(() => expect(result.current.error).toBe("Failed to load file contents"));
    expect(result.current.contents).toBeNull();
    expect(result.current.loading).toBe(false);
  });

  it("does not fetch and clears state while filePath is null", async () => {
    const spy = vi.spyOn(api, "getSessionFileContents").mockResolvedValue(makeContents("a.ts", "alpha"));

    const { result, rerender } = renderHook(
      ({ path }: { path: string | null }) => useFileContents("s1", path, undefined),
      {
        initialProps: { path: null as string | null },
      },
    );

    // No file selected: no request, no loading, empty viewer.
    await act(async () => {
      await new Promise((r) => setTimeout(r, 5));
    });
    expect(spy).not.toHaveBeenCalled();
    expect(result.current.contents).toBeNull();
    expect(result.current.loading).toBe(false);

    // Selecting a file starts the fetch.
    rerender({ path: "a.ts" });
    await waitFor(() => expect(result.current.contents?.new_content).toBe("alpha"));
    expect(spy).toHaveBeenCalledTimes(1);
  });

  it("drops a superseded in-flight response when switching files rapidly", async () => {
    let resolveA: (v: RichFileContentsResponse | null) => void = () => {};
    const spy = vi.spyOn(api, "getSessionFileContents");
    // a.ts fetch hangs until we resolve it manually.
    spy.mockImplementationOnce(() => new Promise((r) => (resolveA = r)));

    const { result, rerender } = renderHook(({ path }) => useFileContents("s1", path, undefined), {
      initialProps: { path: "a.ts" },
    });
    await waitFor(() => expect(spy).toHaveBeenCalledTimes(1));

    // Switch to b.ts before a.ts resolves; b.ts resolves normally and wins.
    spy.mockResolvedValueOnce(makeContents("b.ts", "beta"));
    rerender({ path: "b.ts" });
    await waitFor(() => expect(result.current.contents?.new_content).toBe("beta"));

    // a.ts's late response is stale (reqId superseded) and must be ignored.
    await act(async () => {
      resolveA(makeContents("a.ts", "alpha-late"));
      await new Promise((r) => setTimeout(r, 5));
    });
    expect(result.current.contents?.new_content).toBe("beta");
  });
});
