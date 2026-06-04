// @vitest-environment jsdom
//
// RTL coverage for the diff-comments empty-key fix (#1842). Renders the
// real hook (debounce + pagehide flush included) and asserts the storage
// hygiene contract that the storage-layer tests pin at the saveComments
// boundary:
//   - switching across sessions the user never commented on writes no key
//   - a session with real, unsent comments survives a pagehide flush
// Both fail on the pre-fix tree (empty state was written through, and the
// pagehide flush wrote it again on every tab-away).

import { renderHook, act } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import { useDiffComments } from "./useDiffComments";
import { storageKey } from "../components/diff/comments/storage";

const DEBOUNCE_MS = 200;

beforeEach(() => {
  window.localStorage.clear();
  vi.useFakeTimers();
});
afterEach(() => {
  vi.useRealTimers();
});

describe("useDiffComments storage hygiene (#1842)", () => {
  it("writes no key when switching across never-commented sessions", () => {
    const { rerender } = renderHook(
      ({ id }: { id: string }) => useDiffComments(id),
      { initialProps: { id: "sess-A" } },
    );

    act(() => {
      rerender({ id: "sess-B" });
      vi.advanceTimersByTime(DEBOUNCE_MS);
    });
    act(() => {
      rerender({ id: "sess-C" });
      vi.advanceTimersByTime(DEBOUNCE_MS);
    });

    expect(window.localStorage.getItem(storageKey("sess-A"))).toBeNull();
    expect(window.localStorage.getItem(storageKey("sess-B"))).toBeNull();
    expect(window.localStorage.getItem(storageKey("sess-C"))).toBeNull();
  });

  it("does not persist an empty active session on pagehide / tab-away", () => {
    renderHook(() => useDiffComments("sess-empty"));

    act(() => {
      window.dispatchEvent(new Event("pagehide"));
    });

    expect(window.localStorage.getItem(storageKey("sess-empty"))).toBeNull();
  });

  it("keeps real comments intact across a pagehide flush", () => {
    const { result } = renderHook(() => useDiffComments("sess-real"));

    act(() => {
      result.current.addComment({
        filePath: "src/foo.rs",
        side: "new",
        startLine: 5,
        endLine: 5,
        body: "needs a guard here",
        capturedSnippet: "let x = 1;",
      });
    });
    act(() => {
      window.dispatchEvent(new Event("pagehide"));
    });

    const raw = window.localStorage.getItem(storageKey("sess-real"));
    expect(raw).not.toBeNull();
    const parsed = JSON.parse(raw!) as { comments: { body: string }[] };
    expect(parsed.comments).toHaveLength(1);
    expect(parsed.comments[0].body).toBe("needs a guard here");
  });
});
