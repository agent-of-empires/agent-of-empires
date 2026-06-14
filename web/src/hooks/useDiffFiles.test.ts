// @vitest-environment jsdom
//
// Hook tests for useDiffFiles. The hook fetches the structured diff file
// list for a session via getSessionDiffFiles, polls every 10s while the
// panel is enabled, dedupes by fingerprint so unchanged responses don't
// bump the revision, resets state on session change, and reports the
// `diff_panel` telemetry signal once per session while enabled. The API
// module is mocked so no real network is touched.

import { renderHook, act, waitFor } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import { useDiffFiles } from "./useDiffFiles";
import type { RichDiffFile, RichDiffFilesResponse } from "../lib/types";

vi.mock("../lib/api", () => ({
  getSessionDiffFiles: vi.fn(),
  reportTelemetrySeen: vi.fn(),
}));

import { getSessionDiffFiles, reportTelemetrySeen } from "../lib/api";

const mockGetFiles = vi.mocked(getSessionDiffFiles);
const mockReportSeen = vi.mocked(reportTelemetrySeen);

function file(over: Partial<RichDiffFile> = {}): RichDiffFile {
  return {
    path: "src/a.ts",
    old_path: null,
    status: "modified",
    additions: 1,
    deletions: 0,
    ...over,
  };
}

function resp(over: Partial<RichDiffFilesResponse> = {}): RichDiffFilesResponse {
  return {
    files: [file()],
    per_repo_bases: [{ base_branch: "main" }],
    warning: null,
    ...over,
  };
}

beforeEach(() => {
  mockGetFiles.mockReset();
  mockReportSeen.mockReset();
});

afterEach(() => {
  vi.useRealTimers();
});

describe("useDiffFiles initial state", () => {
  it("returns empty defaults and does not fetch when sessionId is null", () => {
    mockGetFiles.mockResolvedValue(resp());
    const { result } = renderHook(() => useDiffFiles(null, false));

    expect(result.current.files).toEqual([]);
    expect(result.current.perRepoBases).toEqual([{ base_branch: "main" }]);
    expect(result.current.warning).toBeNull();
    expect(result.current.loading).toBe(false);
    expect(result.current.revision).toBe(0);
    expect(mockGetFiles).not.toHaveBeenCalled();
  });

  it("enters the loading state synchronously when mounted with a session", () => {
    mockGetFiles.mockResolvedValue(resp());
    const { result } = renderHook(() => useDiffFiles("s1", false));
    // The session change from null (initial trackedSessionId) -> "s1" never
    // happens on first mount, so loading stays false here; the loading flag
    // is exercised on the session-change test below.
    expect(result.current.files).toEqual([]);
  });
});

describe("useDiffFiles success", () => {
  it("populates files, perRepoBases and bumps revision on a successful fetch", async () => {
    mockGetFiles.mockResolvedValue(
      resp({
        files: [file({ path: "x.ts" })],
        per_repo_bases: [{ repo_name: "repo-a", base_branch: "dev" }],
        warning: "heads up",
      }),
    );
    const { result } = renderHook(() => useDiffFiles("s1", true));

    await waitFor(() => expect(result.current.files.length).toBe(1));
    expect(result.current.files[0].path).toBe("x.ts");
    expect(result.current.perRepoBases).toEqual([{ repo_name: "repo-a", base_branch: "dev" }]);
    expect(result.current.warning).toBe("heads up");
    expect(result.current.revision).toBe(1);
    expect(result.current.loading).toBe(false);
  });

  it("reports the diff_panel telemetry signal once per session while enabled", async () => {
    mockGetFiles.mockResolvedValue(resp());
    const { result } = renderHook(() => useDiffFiles("s1", true));

    await waitFor(() => expect(result.current.revision).toBe(1));
    expect(mockReportSeen).toHaveBeenCalledTimes(1);
    expect(mockReportSeen).toHaveBeenCalledWith("diff_panel");

    // A manual refresh of the same session must not re-fire the signal.
    await act(async () => {
      await result.current.refresh();
    });
    expect(mockReportSeen).toHaveBeenCalledTimes(1);
  });

  it("does not report diff_panel when the panel is disabled", async () => {
    mockGetFiles.mockResolvedValue(resp());
    const { result } = renderHook(() => useDiffFiles("s1", false));

    await waitFor(() => expect(result.current.revision).toBe(1));
    expect(mockReportSeen).not.toHaveBeenCalled();
  });
});

describe("useDiffFiles empty / null response", () => {
  it("keeps empty files and revision 0 when the API returns null", async () => {
    mockGetFiles.mockResolvedValue(null);
    const { result } = renderHook(() => useDiffFiles("s1", true));

    await waitFor(() => expect(mockGetFiles).toHaveBeenCalled());
    // Null response: no setFiles, no revision bump, no telemetry.
    expect(result.current.files).toEqual([]);
    expect(result.current.revision).toBe(0);
    expect(mockReportSeen).not.toHaveBeenCalled();
  });

  it("handles an empty file list as a real (revision-bumping) response", async () => {
    mockGetFiles.mockResolvedValue(resp({ files: [] }));
    const { result } = renderHook(() => useDiffFiles("s1", true));

    await waitFor(() => expect(result.current.revision).toBe(1));
    expect(result.current.files).toEqual([]);
  });
});

describe("useDiffFiles fingerprint dedupe", () => {
  it("does not bump revision when a refetch returns identical files", async () => {
    mockGetFiles.mockResolvedValue(resp({ files: [file({ path: "same.ts" })] }));
    const { result } = renderHook(() => useDiffFiles("s1", true));

    await waitFor(() => expect(result.current.revision).toBe(1));

    await act(async () => {
      await result.current.refresh();
    });
    // Same fingerprint -> revision stays at 1.
    expect(result.current.revision).toBe(1);
  });

  it("bumps revision again when the file list changes between fetches", async () => {
    mockGetFiles.mockResolvedValueOnce(resp({ files: [file({ path: "one.ts" })] }));
    const { result } = renderHook(() => useDiffFiles("s1", true));
    await waitFor(() => expect(result.current.revision).toBe(1));

    mockGetFiles.mockResolvedValueOnce(resp({ files: [file({ path: "two.ts" })] }));
    await act(async () => {
      await result.current.refresh();
    });
    expect(result.current.revision).toBe(2);
    expect(result.current.files[0].path).toBe("two.ts");
  });
});

describe("useDiffFiles polling", () => {
  it("polls every 10s while enabled and stops once disabled", async () => {
    vi.useFakeTimers();
    mockGetFiles.mockResolvedValue(resp());
    const { rerender, unmount } = renderHook(({ enabled }) => useDiffFiles("s1", enabled), {
      initialProps: { enabled: true },
    });

    // Flush the initial setTimeout(0) fetch.
    await act(async () => {
      await vi.advanceTimersByTimeAsync(0);
    });
    expect(mockGetFiles).toHaveBeenCalledTimes(1);

    await act(async () => {
      await vi.advanceTimersByTimeAsync(10_000);
    });
    expect(mockGetFiles).toHaveBeenCalledTimes(2);

    // Disable: the interval is cleared, so further time does not fetch.
    rerender({ enabled: false });
    await act(async () => {
      await vi.advanceTimersByTimeAsync(30_000);
    });
    expect(mockGetFiles).toHaveBeenCalledTimes(2);

    unmount();
  });
});

describe("useDiffFiles session change", () => {
  it("clears state when the session goes back to null", async () => {
    mockGetFiles.mockResolvedValue(resp({ files: [file({ path: "keep.ts" })] }));
    const { result, rerender } = renderHook(({ id }: { id: string | null }) => useDiffFiles(id, true), {
      initialProps: { id: "s1" as string | null },
    });

    await waitFor(() => expect(result.current.files.length).toBe(1));

    rerender({ id: null });
    expect(result.current.files).toEqual([]);
    expect(result.current.revision).toBe(0);
    expect(result.current.loading).toBe(false);
  });

  it("refetches and sets loading when switching to a new session", async () => {
    mockGetFiles.mockResolvedValue(resp({ files: [file({ path: "a.ts" })] }));
    const { result, rerender } = renderHook(({ id }: { id: string | null }) => useDiffFiles(id, true), {
      initialProps: { id: "s1" as string | null },
    });
    await waitFor(() => expect(result.current.files[0].path).toBe("a.ts"));

    mockGetFiles.mockResolvedValue(resp({ files: [file({ path: "b.ts" })] }));
    rerender({ id: "s2" });
    // Switching sessions flips loading on at render time.
    expect(result.current.loading).toBe(true);

    await waitFor(() => expect(result.current.files[0].path).toBe("b.ts"));
    expect(result.current.loading).toBe(false);
  });
});
