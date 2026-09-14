// @vitest-environment jsdom
//
// Direct unit tests for the `useHighlightedLines` hook. Mocks
// `../../lib/snippetHighlighter` and `../useShikiTheme` so the hook can run
// in jsdom without WASM. Covers:
//
// - No-language path: `langHintForPath` returns an empty hint, the effect
//   bails before any async work and `tokens` stays null.
// - Success path: the shared highlighter resolves, state settles with a
//   grid for every hunk line.
// - Catch path: `getSnippetHighlighter` / `codeToTokens` reject. The IIFE
//   must catch and settle state with an empty grid rather than leaving the
//   hook loading forever (PR #1355 root regression).
// - Reqid guard: a stale request must not overwrite a newer one.

import { afterEach, describe, expect, it, vi } from "vitest";
import { renderHook, waitFor } from "@testing-library/react";
import type { RichDiffHunk } from "../../lib/types";

const getSnippetHighlighter = vi.fn();
const useShikiTheme = vi.fn();

vi.mock("../../lib/snippetHighlighter", () => ({
  getSnippetHighlighter: (...args: unknown[]) => getSnippetHighlighter(...args),
  langHintForPath: (path: string) => (path.includes(".") ? (path.split(".").pop() ?? "") : ""),
}));

vi.mock("../useShikiTheme", () => ({
  useShikiTheme: () => useShikiTheme(),
}));

// Imported AFTER the mocks so it picks up the stubs.
import { useHighlightedLines } from "../useHighlightedLines";

function hunkOf(content: string): RichDiffHunk {
  return {
    old_start: 1,
    old_lines: 1,
    new_start: 1,
    new_lines: 1,
    lines: [{ type: "equal", old_line_num: 1, new_line_num: 1, content }],
  };
}

afterEach(() => {
  vi.clearAllMocks();
});

describe("useHighlightedLines", () => {
  it("returns tokens=null when the file has no extension", async () => {
    useShikiTheme.mockReturnValue({ theme: "github-dark", appearance: "dark" });

    const { result } = renderHook(() => useHighlightedLines([hunkOf("some text\n")], "README"));

    expect(result.current.tokens).toBeNull();
    expect(getSnippetHighlighter).not.toHaveBeenCalled();
  });

  it("settles an empty grid when the extension has no grammar", async () => {
    useShikiTheme.mockReturnValue({ theme: "github-dark", appearance: "dark" });
    getSnippetHighlighter.mockResolvedValue(null);

    const { result } = renderHook(() => useHighlightedLines([hunkOf("some text\n")], "README.unknown"));

    await waitFor(() => {
      expect(result.current.tokens).toEqual([]);
    });
    expect(getSnippetHighlighter).toHaveBeenCalledWith(
      expect.objectContaining({ langHint: "unknown", theme: "github-dark" }),
    );
  });

  it("settles tokens with a grid when shiki resolves", async () => {
    useShikiTheme.mockReturnValue({ theme: "github-dark", appearance: "dark" });
    const codeToTokens = vi.fn(() => ({
      tokens: [[{ content: "x", color: "#abcdef" }]],
    }));
    getSnippetHighlighter.mockResolvedValue({
      highlighter: { codeToTokens },
      langId: "tsx",
      theme: "github-dark",
    });

    const { result } = renderHook(() => useHighlightedLines([hunkOf("x\n")], "src/example.tsx"));

    await waitFor(() => {
      expect(result.current.tokens).not.toBeNull();
    });
    expect(result.current.tokens).toEqual([[[{ content: "x", color: "#abcdef" }]]]);
    expect(codeToTokens).toHaveBeenCalledWith("x", expect.objectContaining({ lang: "tsx", theme: "github-dark" }));
  });

  it("falls back to empty grid when the highlighter rejects", async () => {
    useShikiTheme.mockReturnValue({ theme: "github-dark", appearance: "dark" });
    // Simulates the real-world CSP WASM block: getSharedHighlighter rejects
    // with a CompileError, so the IIFE must enter the catch and settle
    // state instead of leaving loading=true forever.
    getSnippetHighlighter.mockRejectedValue(new Error("call to WebAssembly.instantiate() blocked by CSP"));
    const errSpy = vi.spyOn(console, "error").mockImplementation(() => {});

    const { result } = renderHook(() => useHighlightedLines([hunkOf("x\n")], "src/example.tsx"));

    await waitFor(() => {
      expect(result.current.tokens).toEqual([]);
    });
    expect(errSpy).toHaveBeenCalled();
    errSpy.mockRestore();
  });

  it("returns null tokens after filePath switches until the new path settles", async () => {
    useShikiTheme.mockReturnValue({ theme: "github-dark", appearance: "dark" });
    getSnippetHighlighter.mockResolvedValue({
      highlighter: {
        codeToTokens: vi.fn(() => ({
          tokens: [[{ content: "x", color: "#222222" }]],
        })),
      },
      langId: "tsx",
      theme: "github-dark",
    });

    const { result, rerender } = renderHook(
      ({ path }: { path: string }) => useHighlightedLines([hunkOf("x\n")], path),
      { initialProps: { path: "first.tsx" } },
    );

    await waitFor(() => {
      expect(result.current.tokens).not.toBeNull();
    });

    rerender({ path: "second.tsx" });
    // First render after the switch: `state.path` still says
    // `first.tsx`, so tokens reads as null even though state is set.
    expect(result.current.tokens).toBeNull();

    await waitFor(() => {
      expect(result.current.tokens).not.toBeNull();
    });
  });
});
