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
  it("returns the first cap items unfiltered for an empty query", () => {
    const items: Item[] = [
      { label: "a" },
      { label: "b" },
      { label: "c" },
      { label: "d" },
    ];
    expect(fuzzyFilter(items, "", 2)).toEqual([{ label: "a" }, { label: "b" }]);
  });

  it("ranks prefix match above substring match", () => {
    const items: Item[] = [{ label: "zfoo" }, { label: "foobar" }];
    const out = fuzzyFilter(items, "foo");
    expect(out.map((i) => i.label)).toEqual(["foobar", "zfoo"]);
  });

  it("label substring beats description substring", () => {
    const items: Item[] = [
      { label: "other", description: "contains foo in description" },
      { label: "has-foo-in-label" },
    ];
    const out = fuzzyFilter(items, "foo");
    expect(out.map((i) => i.label)).toEqual(["has-foo-in-label", "other"]);
  });

  it("breaks score ties by shorter label", () => {
    const items: Item[] = [
      { label: "foobarbaz" },
      { label: "foobar" },
      { label: "foo" },
    ];
    const out = fuzzyFilter(items, "foo");
    expect(out.map((i) => i.label)).toEqual(["foo", "foobar", "foobarbaz"]);
  });

  it("caps results at the cap limit", () => {
    const items: Item[] = Array.from({ length: 50 }, (_, i) => ({
      label: `foo${i}`,
    }));
    expect(fuzzyFilter(items, "foo", 5)).toHaveLength(5);
  });

  it("drops items that don't match label or description", () => {
    const items: Item[] = [
      { label: "alpha" },
      { label: "beta", description: "the quick" },
      { label: "gamma" },
    ];
    expect(fuzzyFilter(items, "xyz")).toEqual([]);
  });

  it("is case-insensitive", () => {
    const items: Item[] = [{ label: "README.md" }];
    expect(fuzzyFilter(items, "readme")).toEqual([{ label: "README.md" }]);
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

  it("returns loading=true on first render and resolves with files", async () => {
    fetchSpy.mockResolvedValueOnce({
      ok: true,
      json: async () => ({ files: ["a.txt", "b.rs"] }),
    });
    const { result } = renderHook(() => useFilesIndex("s-1"));
    expect(result.current.loading).toBe(true);
    expect(result.current.files).toEqual([]);
    await waitFor(() => expect(result.current.loading).toBe(false));
    expect(result.current.files).toEqual(["a.txt", "b.rs"]);
    expect(fetchSpy).toHaveBeenCalledWith("/api/sessions/s-1/acp/files");
  });

  it("URL-encodes the session id", async () => {
    fetchSpy.mockResolvedValueOnce({
      ok: true,
      json: async () => ({ files: [] }),
    });
    renderHook(() => useFilesIndex("session/with/slash"));
    await waitFor(() =>
      expect(fetchSpy).toHaveBeenCalledWith(
        "/api/sessions/session%2Fwith%2Fslash/acp/files",
      ),
    );
  });

  it("re-fetches when sessionId changes", async () => {
    fetchSpy
      .mockResolvedValueOnce({
        ok: true,
        json: async () => ({ files: ["one"] }),
      })
      .mockResolvedValueOnce({
        ok: true,
        json: async () => ({ files: ["two"] }),
      });
    const { result, rerender } = renderHook(
      ({ id }: { id: string }) => useFilesIndex(id),
      { initialProps: { id: "s-1" } },
    );
    await waitFor(() => expect(result.current.files).toEqual(["one"]));
    rerender({ id: "s-2" });
    await waitFor(() => expect(result.current.files).toEqual(["two"]));
    expect(fetchSpy).toHaveBeenCalledTimes(2);
  });

  it("returns empty list when fetch rejects", async () => {
    fetchSpy.mockRejectedValueOnce(new Error("network down"));
    const { result } = renderHook(() => useFilesIndex("s-1"));
    await waitFor(() => expect(result.current.loading).toBe(false));
    expect(result.current.files).toEqual([]);
  });

  it("treats non-ok responses as an empty file list", async () => {
    fetchSpy.mockResolvedValueOnce({
      ok: false,
      json: async () => ({ files: ["should-not-appear"] }),
    });
    const { result } = renderHook(() => useFilesIndex("s-1"));
    await waitFor(() => expect(result.current.loading).toBe(false));
    expect(result.current.files).toEqual([]);
  });

  it("tolerates a response without a files field", async () => {
    fetchSpy.mockResolvedValueOnce({
      ok: true,
      json: async () => ({}),
    });
    const { result } = renderHook(() => useFilesIndex("s-1"));
    await waitFor(() => expect(result.current.loading).toBe(false));
    expect(result.current.files).toEqual([]);
  });

  it("does not update state after unmount", async () => {
    let resolveFetch: (v: unknown) => void = () => {};
    const pending = new Promise<unknown>((res) => {
      resolveFetch = res;
    });
    fetchSpy.mockReturnValueOnce(pending);
    const { result, unmount } = renderHook(() => useFilesIndex("s-1"));
    unmount();
    await act(async () => {
      resolveFetch({ ok: true, json: async () => ({ files: ["x"] }) });
    });
    expect(result.current.files).toEqual([]);
  });
});
