// @vitest-environment jsdom
//
// Direct unit tests for the `useHighlightedLines` hook. Mocks
// `../../lib/highlighter` and `../useShikiTheme` so the hook can run
// in jsdom without WASM. Covers:
//
// - No-language path: `langImportForPath` returns null, the effect
//   bails before any async work and `tokens` stays null.
// - Success path: theme + grammar + tokens resolve, state settles
//   with a grid for every hunk line.
// - Catch path: ensureThemeLoaded / getHighlighter / langImport /
//   loadLanguage / codeToTokens reject. The IIFE must catch and
//   settle state with an empty grid rather than leaving the hook
//   loading forever (PR #1355 root regression).
// - Reqid guard: a stale request must not overwrite a newer one.

import { afterEach, describe, expect, it, vi } from "vitest";
import { renderHook, waitFor } from "@testing-library/react";
import type { RichDiffHunk } from "../../lib/types";

const ensureThemeLoaded = vi.fn();
const getHighlighter = vi.fn();
const langImportForPath = vi.fn();
const useShikiTheme = vi.fn();

vi.mock("../../lib/highlighter", () => ({
  ensureThemeLoaded: (...args: unknown[]) => ensureThemeLoaded(...args),
  getHighlighter: () => getHighlighter(),
  langImportForPath: (path: string) => langImportForPath(path),
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
  it("returns tokens=null when the file extension has no grammar", async () => {
    useShikiTheme.mockReturnValue({ theme: "github-dark", appearance: "dark" });
    langImportForPath.mockReturnValue(null);

    const { result } = renderHook(() =>
      useHighlightedLines([hunkOf("some text\n")], "README.unknown"),
    );

    expect(result.current.tokens).toBeNull();
    expect(ensureThemeLoaded).not.toHaveBeenCalled();
    expect(getHighlighter).not.toHaveBeenCalled();
  });

  it("settles tokens with a grid when shiki resolves", async () => {
    useShikiTheme.mockReturnValue({ theme: "github-dark", appearance: "dark" });
    const langModule = { default: [{ name: "tsx" }] };
    langImportForPath.mockReturnValue(() => Promise.resolve(langModule));
    ensureThemeLoaded.mockResolvedValue("github-dark");
    const codeToTokens = vi.fn(() => ({
      tokens: [[{ content: "x", color: "#abcdef" }]],
    }));
    getHighlighter.mockResolvedValue({
      getLoadedLanguages: () => [],
      loadLanguage: vi.fn().mockResolvedValue(undefined),
      codeToTokens,
    });

    const { result } = renderHook(() =>
      useHighlightedLines([hunkOf("x\n")], "src/example.tsx"),
    );

    await waitFor(() => {
      expect(result.current.tokens).not.toBeNull();
    });
    expect(result.current.tokens).toEqual([
      [[{ content: "x", color: "#abcdef" }]],
    ]);
    expect(codeToTokens).toHaveBeenCalledWith(
      "x",
      expect.objectContaining({ lang: "tsx", theme: "github-dark" }),
    );
  });

  it("falls back to empty grid when the highlighter rejects", async () => {
    useShikiTheme.mockReturnValue({ theme: "github-dark", appearance: "dark" });
    langImportForPath.mockReturnValue(() =>
      Promise.resolve({ default: [{ name: "tsx" }] }),
    );
    ensureThemeLoaded.mockResolvedValue("github-dark");
    // Simulates the real-world CSP WASM block: createHighlighterCore
    // rejects with a CompileError, so the IIFE must enter the catch
    // and settle state instead of leaving loading=true forever.
    getHighlighter.mockRejectedValue(
      new Error("call to WebAssembly.instantiate() blocked by CSP"),
    );
    const errSpy = vi.spyOn(console, "error").mockImplementation(() => {});

    const { result } = renderHook(() =>
      useHighlightedLines([hunkOf("x\n")], "src/example.tsx"),
    );

    await waitFor(() => {
      expect(result.current.tokens).toEqual([]);
    });
    expect(errSpy).toHaveBeenCalled();
    errSpy.mockRestore();
  });

  it("treats unknown language registration as success with empty grid", async () => {
    // A registered grammar without a `name` field would otherwise leave
    // the hook stuck. The hook should still settle state (empty grid)
    // so the caller renders raw text.
    useShikiTheme.mockReturnValue({ theme: "github-dark", appearance: "dark" });
    langImportForPath.mockReturnValue(() =>
      Promise.resolve({ default: [{ /* missing name */ patterns: [] }] }),
    );
    ensureThemeLoaded.mockResolvedValue("github-dark");
    getHighlighter.mockResolvedValue({
      getLoadedLanguages: () => [],
      loadLanguage: vi.fn().mockResolvedValue(undefined),
      codeToTokens: vi.fn(),
    });

    const { result } = renderHook(() =>
      useHighlightedLines([hunkOf("x\n")], "src/example.tsx"),
    );

    await waitFor(() => {
      expect(result.current.tokens).toEqual([]);
    });
  });

  it("returns null tokens after filePath switches until the new path settles", async () => {
    useShikiTheme.mockReturnValue({ theme: "github-dark", appearance: "dark" });
    langImportForPath.mockReturnValue(() =>
      Promise.resolve({ default: [{ name: "tsx" }] }),
    );
    ensureThemeLoaded.mockResolvedValue("github-dark");
    getHighlighter.mockResolvedValue({
      getLoadedLanguages: () => [],
      loadLanguage: vi.fn().mockResolvedValue(undefined),
      codeToTokens: vi.fn(() => ({
        tokens: [[{ content: "x", color: "#222222" }]],
      })),
    });

    const { result, rerender } = renderHook(
      ({ path }: { path: string }) =>
        useHighlightedLines([hunkOf("x\n")], path),
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
