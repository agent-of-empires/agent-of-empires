// @vitest-environment jsdom

import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { renderHook, waitFor } from "@testing-library/react";

vi.mock("../../../lib/api", () => ({
  fetchSessions: vi.fn(() =>
    Promise.resolve({ sessions: [], workspace_ordering: [], cursor: { epoch: "boot", revision: 1n } }),
  ),
  fetchRecentProjects: vi.fn(() => Promise.resolve({ projects: [] })),
  fetchProjects: vi.fn(() => Promise.resolve([])),
}));

import { fetchProjects } from "../../../lib/api";
import { useProjectPicker } from "../steps/projectPicker";

beforeEach(() => {
  vi.mocked(fetchProjects).mockResolvedValue([]);
});

afterEach(() => {
  vi.clearAllMocks();
});

describe("useProjectPicker saved-projects read", () => {
  it("never sends a saved-projects read with no scope to carry", async () => {
    // The daemon answers an unscoped /api/projects read with 400
    // profile_required, so the unresolved-profile load must not ask.
    const { result } = renderHook(() => useProjectPicker(undefined));

    await waitFor(() => expect(result.current.loading).toBe(false));
    expect(vi.mocked(fetchProjects)).not.toHaveBeenCalled();
    // The scope-free recents still load, so the step is usable while the
    // wizard has no profile yet.
    expect(result.current.error).toBe(false);
  });

  it("carries the profile on the read once one is resolved", async () => {
    const { result } = renderHook(() => useProjectPicker("work"));

    await waitFor(() => expect(vi.mocked(fetchProjects)).toHaveBeenCalledWith({ profile: "work" }));
    await waitFor(() => expect(result.current.loading).toBe(false));
    expect(result.current.error).toBe(false);
  });
});
