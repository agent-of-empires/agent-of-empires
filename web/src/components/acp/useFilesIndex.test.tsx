// @vitest-environment jsdom
//
// Workspace file picker hook + fuzzy filter. The fuzzy filter drives
// the @-mention picker's ordering; if prefix-vs-substring weighting
// breaks, the picker stops surfacing the file the user is typing.

import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { act, cleanup, renderHook, waitFor } from "@testing-library/react";

import { fuzzyFilter, useFilesIndex } from "./useFilesIndex";

interface Item {
  label: string;
  description?: string;
}

describe("fuzzyFilter", () => {
  it("ranks prefix, label, and shorter matches first, case-insensitively, within the cap", () => {
    const labels = (items: Item[], query: string, cap?: number) => fuzzyFilter(items, query, cap).map((i) => i.label);
    // Empty query returns the first `cap` items unfiltered.
    expect(labels([{ label: "a" }, { label: "b" }, { label: "c" }], "", 2)).toEqual(["a", "b"]);
    expect(labels([{ label: "zfoo" }, { label: "foobar" }], "foo")).toEqual(["foobar", "zfoo"]);
    expect(
      labels([{ label: "other", description: "contains foo in description" }, { label: "has-foo-in-label" }], "foo"),
    ).toEqual(["has-foo-in-label", "other"]);
    expect(labels([{ label: "foobarbaz" }, { label: "foobar" }, { label: "foo" }], "foo")).toEqual([
      "foo",
      "foobar",
      "foobarbaz",
    ]);
    expect(
      labels(
        Array.from({ length: 50 }, (_, i) => ({ label: `foo${i}` })),
        "foo",
        5,
      ),
    ).toHaveLength(5);
    expect(labels([{ label: "alpha" }, { label: "beta", description: "the quick" }], "xyz")).toEqual([]);
    expect(labels([{ label: "README.md" }], "readme")).toEqual(["README.md"]);
  });
});

describe("useFilesIndex hook", () => {
  let fetchSpy: ReturnType<typeof vi.fn>;

  beforeEach(() => {
    fetchSpy = vi.fn();
    vi.stubGlobal("fetch", fetchSpy);
  });

  afterEach(() => {
    cleanup();
    vi.unstubAllGlobals();
  });

  it("starts loading, then resolves with files from the URL-encoded session path", async () => {
    fetchSpy.mockResolvedValueOnce({ ok: true, json: async () => ({ files: ["a.txt", "b.rs"] }) });
    const { result } = renderHook(() => useFilesIndex("session/with/slash"));
    expect(result.current.loading).toBe(true);
    expect(result.current.files).toEqual([]);
    await waitFor(() => expect(result.current.loading).toBe(false));
    expect(result.current.files).toEqual(["a.txt", "b.rs"]);
    expect(fetchSpy).toHaveBeenCalledWith("/api/sessions/session%2Fwith%2Fslash/acp/files");
  });

  it("re-fetches when sessionId changes", async () => {
    fetchSpy
      .mockResolvedValueOnce({ ok: true, json: async () => ({ files: ["one"] }) })
      .mockResolvedValueOnce({ ok: true, json: async () => ({ files: ["two"] }) });
    const { result, rerender } = renderHook(({ id }: { id: string }) => useFilesIndex(id), {
      initialProps: { id: "s-1" },
    });
    await waitFor(() => expect(result.current.files).toEqual(["one"]));
    rerender({ id: "s-2" });
    await waitFor(() => expect(result.current.files).toEqual(["two"]));
    expect(fetchSpy).toHaveBeenCalledTimes(2);
  });

  it.each([
    ["a rejected fetch", () => fetchSpy.mockRejectedValueOnce(new Error("network down"))],
    [
      "a non-ok response",
      () => fetchSpy.mockResolvedValueOnce({ ok: false, json: async () => ({ files: ["should-not-appear"] }) }),
    ],
    ["a response without files", () => fetchSpy.mockResolvedValueOnce({ ok: true, json: async () => ({}) })],
  ])("yields an empty list for %s", async (_label, arrange) => {
    arrange();
    const { result } = renderHook(() => useFilesIndex("s-1"));
    await waitFor(() => expect(result.current.loading).toBe(false));
    expect(result.current.files).toEqual([]);
  });

  it.each(["success", "failure"] as const)("ignores a superseded session's late %s", async (outcome) => {
    let resolveFetch!: (value: Response) => void;
    let rejectFetch!: (reason: Error) => void;
    const pending = new Promise<Response>((resolve, reject) => {
      resolveFetch = resolve;
      rejectFetch = reject;
    });
    fetchSpy.mockReturnValueOnce(pending).mockResolvedValueOnce({
      ok: true,
      json: async () => ({ files: ["current.txt"] }),
    });
    const { result, rerender } = renderHook(({ id }) => useFilesIndex(id), {
      initialProps: { id: "old-session" },
    });
    rerender({ id: "current-session" });
    await waitFor(() => expect(result.current.files).toEqual(["current.txt"]));

    await act(async () => {
      if (outcome === "success") {
        resolveFetch(new Response(JSON.stringify({ files: ["stale.txt"] })));
      } else {
        rejectFetch(new Error("old session unavailable"));
      }
    });
    expect(result.current.files).toEqual(["current.txt"]);
    expect(result.current.error).toBe(false);
    expect(result.current.loading).toBe(false);
  });

  it("keeps the current session loading when a superseded request completes", async () => {
    let resolveOld!: (value: Response) => void;
    let resolveCurrent!: (value: Response) => void;
    fetchSpy
      .mockReturnValueOnce(
        new Promise<Response>((resolve) => {
          resolveOld = resolve;
        }),
      )
      .mockReturnValueOnce(
        new Promise<Response>((resolve) => {
          resolveCurrent = resolve;
        }),
      );
    const { result, rerender } = renderHook(({ id }) => useFilesIndex(id), {
      initialProps: { id: "old-session" },
    });
    rerender({ id: "current-session" });
    await act(async () => {
      resolveOld(new Response(JSON.stringify({ files: ["stale.txt"] })));
    });
    expect(result.current.loading).toBe(true);
    expect(result.current.files).toEqual([]);
    await act(async () => {
      resolveCurrent(new Response(JSON.stringify({ files: ["current.txt"] })));
    });
    expect(result.current.loading).toBe(false);
    expect(result.current.files).toEqual(["current.txt"]);
  });
});
