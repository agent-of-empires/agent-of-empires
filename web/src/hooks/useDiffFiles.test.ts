// @vitest-environment jsdom

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

const file = (path = "src/a.ts"): RichDiffFile => ({
  path,
  old_path: null,
  status: "modified",
  additions: 1,
  deletions: 0,
});

const resp = (over: Partial<RichDiffFilesResponse> = {}): RichDiffFilesResponse => ({
  files: [file()],
  per_repo_bases: [{ base_branch: "main" }],
  warning: null,
  ...over,
});

beforeEach(() => {
  mockGetFiles.mockReset();
  mockReportSeen.mockReset();
});

afterEach(() => vi.useRealTimers());

async function mountLoaded(first: RichDiffFilesResponse, enabled = true) {
  mockGetFiles.mockResolvedValueOnce(first);
  const hook = renderHook(() => useDiffFiles("s1", enabled));
  await waitFor(() => expect(hook.result.current.revision).toBe(1));
  const refetch = async (next: RichDiffFilesResponse) => {
    mockGetFiles.mockResolvedValueOnce(next);
    await act(() => hook.result.current.refresh());
  };
  return { ...hook, refetch };
}

describe("useDiffFiles", () => {
  it("returns defaults and does not fetch without a session", () => {
    const { result } = renderHook(() => useDiffFiles(null, false));
    expect(result.current).toMatchObject({
      files: [],
      perRepoBases: [{ base_branch: "main", repo_path: "" }],
      warning: null,
      loading: false,
      revision: 0,
    });
    expect(mockGetFiles).not.toHaveBeenCalled();
  });

  it("reports diff_panel once per session, and only while the panel is enabled", async () => {
    const enabled = await mountLoaded(resp());
    await enabled.refetch(resp({ files: [file("b.ts")] }));
    expect(mockReportSeen.mock.calls).toEqual([["diff_panel"]]);

    mockReportSeen.mockReset();
    await mountLoaded(resp(), false);
    expect(mockReportSeen).not.toHaveBeenCalled();
  });

  it("keeps revision 0 on a null response, but counts an empty list", async () => {
    mockGetFiles.mockResolvedValue(null);
    const { result } = renderHook(() => useDiffFiles("s1", true));
    await waitFor(() => expect(mockGetFiles).toHaveBeenCalled());
    expect(result.current).toMatchObject({ files: [], revision: 0 });
    expect(mockReportSeen).not.toHaveBeenCalled();

    await mountLoaded(resp({ files: [] }));
  });

  it("bumps revision only when the fingerprint changes (#3329)", async () => {
    const files = [file("same.ts")];
    const { result, refetch } = await mountLoaded(resp({ files }));

    await refetch(resp({ files }));
    expect(result.current.revision).toBe(1);

    const override = {
      repo_name: "api",
      base_branch: "epic/checkout",
      repo_path: "/ws/api",
      base_override: "epic/checkout",
    };
    await refetch(resp({ files, per_repo_bases: [override] }));
    expect(result.current.perRepoBases).toEqual([override]);

    await refetch(resp({ files, per_repo_bases: [override], warning: "api: no merge base" }));
    expect(result.current.warning).toBe("api: no merge base");

    await refetch(resp({ files: [file("two.ts")], per_repo_bases: [override], warning: "api: no merge base" }));
    expect(result.current.revision).toBe(4);
    expect(result.current.files[0]!.path).toBe("two.ts");
  });

  it("polls every 10s while enabled and stops once disabled", async () => {
    vi.useFakeTimers();
    mockGetFiles.mockResolvedValue(resp());
    const { rerender } = renderHook(({ enabled }) => useDiffFiles("s1", enabled), { initialProps: { enabled: true } });
    const tick = (ms: number) => act(() => vi.advanceTimersByTimeAsync(ms));

    await tick(0);
    expect(mockGetFiles).toHaveBeenCalledTimes(1);
    await tick(10_000);
    expect(mockGetFiles).toHaveBeenCalledTimes(2);
    rerender({ enabled: false });
    await tick(30_000);
    expect(mockGetFiles).toHaveBeenCalledTimes(2);
  });

  it("clears on a null session and reloads on a new one", async () => {
    mockGetFiles.mockResolvedValue(resp({ files: [file("a.ts")] }));
    const { result, rerender } = renderHook(({ id }: { id: string | null }) => useDiffFiles(id, true), {
      initialProps: { id: "s1" as string | null },
    });
    await waitFor(() => expect(result.current.files[0]?.path).toBe("a.ts"));

    mockGetFiles.mockResolvedValue(resp({ files: [file("b.ts")] }));
    rerender({ id: "s2" });
    expect(result.current.loading).toBe(true);
    await waitFor(() => expect(result.current.files[0]?.path).toBe("b.ts"));
    expect(result.current.loading).toBe(false);

    rerender({ id: null });
    expect(result.current).toMatchObject({ files: [], revision: 0, loading: false });
  });
});
