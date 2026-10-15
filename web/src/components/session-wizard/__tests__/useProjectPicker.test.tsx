// @vitest-environment jsdom

import { beforeEach, describe, expect, it, vi } from "vitest";
import { act, renderHook, waitFor } from "@testing-library/react";

import { useProjectPicker } from "../steps/projectPicker";
import type { ProjectInfo } from "../../../lib/types";

const fetchProjectRegistry = vi.fn();

vi.mock("../../../lib/api", () => ({
  fetchSessions: vi.fn().mockResolvedValue({ sessions: [] }),
  fetchRecentProjects: vi.fn().mockResolvedValue({ projects: [] }),
  fetchProjectRegistry: (...args: unknown[]) => fetchProjectRegistry(...args),
}));

const project = (name: string): ProjectInfo => ({ name, path: `/repo/${name}`, scope: "global", pinned: false });

describe("useProjectPicker profile", () => {
  beforeEach(() => {
    vi.clearAllMocks();
  });

  it("reports loading, not the previous profile's projects, until the new profile's list lands", async () => {
    let resolveB!: (projects: ProjectInfo[]) => void;
    fetchProjectRegistry.mockImplementation((_scope?: string, profile?: string) =>
      profile === "b" ? new Promise((resolve) => (resolveB = resolve)) : Promise.resolve([project("alpha")]),
    );
    const { result, rerender } = renderHook(({ profile }) => useProjectPicker([], profile), {
      initialProps: { profile: "a" },
    });
    await waitFor(() => expect(result.current.loading).toBe(false));
    expect(result.current.saved.map((p) => p.name)).toEqual(["alpha"]);

    rerender({ profile: "b" });
    await waitFor(() => expect(fetchProjectRegistry).toHaveBeenCalledWith(undefined, "b"));
    expect(result.current.loading).toBe(true);

    await act(async () => resolveB([project("beta")]));
    await waitFor(() => expect(result.current.loading).toBe(false));
    expect(result.current.saved.map((p) => p.name)).toEqual(["beta"]);
  });

  it("tells a failed registry request apart from an empty registry", async () => {
    fetchProjectRegistry.mockResolvedValueOnce([]).mockResolvedValueOnce(null);
    const empty = renderHook(() => useProjectPicker());
    await waitFor(() => expect(empty.result.current.loading).toBe(false));
    expect(empty.result.current.registryAvailable).toBe(true);

    const failed = renderHook(() => useProjectPicker());
    await waitFor(() => expect(failed.result.current.loading).toBe(false));
    expect(failed.result.current.registryAvailable).toBe(false);
    expect(failed.result.current.saved).toEqual([]);
  });
});
