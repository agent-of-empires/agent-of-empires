// @vitest-environment jsdom

import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
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

beforeEach(() => {
  useShikiTheme.mockReturnValue({ theme: "github-dark", appearance: "dark" });
});

afterEach(() => {
  vi.clearAllMocks();
});

describe("useHighlightedLines", () => {
  it("returns tokens=null when the file has no extension", () => {
    const { result } = renderHook(() => useHighlightedLines([hunkOf("some text\n")], "README"));

    expect(result.current.tokens).toBeNull();
    expect(getSnippetHighlighter).not.toHaveBeenCalled();
  });

  it("settles an empty grid when the extension has no grammar", async () => {
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
    expect(result.current.tokens).toBeNull();

    await waitFor(() => {
      expect(result.current.tokens).not.toBeNull();
    });
  });
});
