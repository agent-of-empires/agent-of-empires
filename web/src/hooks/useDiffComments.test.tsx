// @vitest-environment jsdom

import { renderHook, act } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import { useDiffComments } from "./useDiffComments";
import { storageKey } from "../components/diff/comments/storage";

const DEBOUNCE_MS = 200;

const draft = (body: string) => ({
  filePath: "src/foo.rs",
  side: "new" as const,
  startLine: 5,
  endLine: 5,
  body,
  capturedSnippet: "let x = 1;",
});

beforeEach(() => {
  window.localStorage.clear();
  vi.useFakeTimers();
});
afterEach(() => {
  vi.useRealTimers();
});

describe("useDiffComments", () => {
  it("keeps each session's comments and drafts to itself across switches", () => {
    const { result, rerender } = renderHook(({ id }: { id: string }) => useDiffComments(id), {
      initialProps: { id: "sess-A" },
    });

    act(() => result.current.addComment(draft("from A")));
    act(() => result.current.setIntroDraft("intro A"));
    act(() => vi.advanceTimersByTime(DEBOUNCE_MS));

    act(() => rerender({ id: "sess-B" }));
    expect(result.current.comments).toHaveLength(0);
    expect(result.current.introDraft).toBe("");

    act(() => result.current.addComment(draft("from B")));
    act(() => vi.advanceTimersByTime(DEBOUNCE_MS));

    act(() => rerender({ id: "sess-A" }));
    expect(result.current.comments.map((c) => c.body)).toEqual(["from A"]);
    expect(result.current.introDraft).toBe("intro A");
  });

  // #1842: browsing sessions nobody commented on must not leave storage keys behind.
  it("writes no key when switching across never-commented sessions", () => {
    const { rerender } = renderHook(({ id }: { id: string }) => useDiffComments(id), {
      initialProps: { id: "sess-A" },
    });

    for (const id of ["sess-B", "sess-C"]) {
      act(() => {
        rerender({ id });
        vi.advanceTimersByTime(DEBOUNCE_MS);
      });
    }

    for (const id of ["sess-A", "sess-B", "sess-C"]) {
      expect(window.localStorage.getItem(storageKey(id))).toBeNull();
    }
  });

  it("flushes real comments on pagehide but leaves an empty session unwritten", () => {
    const { result } = renderHook(({ id }: { id: string }) => useDiffComments(id), {
      initialProps: { id: "sess-empty" },
    });
    act(() => window.dispatchEvent(new Event("pagehide")));
    expect(window.localStorage.getItem(storageKey("sess-empty"))).toBeNull();

    const { result: real } = renderHook(() => useDiffComments("sess-real"));
    act(() => real.current.addComment(draft("needs a guard here")));
    act(() => window.dispatchEvent(new Event("pagehide")));

    const raw = window.localStorage.getItem(storageKey("sess-real"));
    expect(raw).not.toBeNull();
    expect(JSON.parse(raw!).comments).toEqual([expect.objectContaining({ body: "needs a guard here" })]);
    expect(result.current.comments).toHaveLength(0);
  });
});
