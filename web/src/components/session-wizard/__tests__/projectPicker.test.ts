import { describe, expect, it } from "vitest";

import { collectRecentProjects, mergeRecentProjects, splitSavedAndRecent } from "../steps/projectPicker";
import type { RecentProjectEntry } from "../../../lib/api";
import type { ProjectInfo } from "../../../lib/types";
import { mockSession } from "./fixtures";

const persisted = (
  path: string,
  last_used_at = "2025-09-01T00:00:00+00:00",
  display_name = "",
): RecentProjectEntry => ({
  path,
  display_name,
  tool: "claude",
  last_used_at,
});
const saved = (path: string): ProjectInfo => ({ name: "p", path, scope: "global", pinned: false }) as ProjectInfo;

describe("collectRecentProjects", () => {
  it("collapses trailing-slash variants into one entry with a summed count", () => {
    const recents = collectRecentProjects([
      mockSession({ id: "a", project_path: "/foo/bar/", last_accessed_at: "2025-09-01T00:00:00Z" }),
      mockSession({ id: "b", project_path: "/foo/bar///" }),
      mockSession({ id: "c", project_path: "/foo/bar", last_accessed_at: "2025-09-02T00:00:00Z" }),
      mockSession({ id: "d", project_path: "/repo/beta/" }),
    ]);
    expect(recents.map((r) => [r.path, r.displayName, r.sessionCount])).toEqual([
      ["/foo/bar", "bar", 3],
      ["/repo/beta", "beta", 1],
    ]);
  });

  it("keeps the filesystem root as its own name", () => {
    expect(collectRecentProjects([mockSession({ project_path: "/" })])).toMatchObject([
      { path: "/", displayName: "/" },
    ]);
  });

  it("skips scratch and multi-repo workspace sessions", () => {
    const recents = collectRecentProjects([
      mockSession({ id: "real", project_path: "/repo/alpha" }),
      mockSession({ id: "scratch", project_path: "/app/scratch/aaa", scratch: true }),
      mockSession({
        id: "ws",
        project_path: "/repo/gamma",
        main_repo_path: "/repo/gamma",
        workspace_repos: [{ name: "gamma", source_path: "/repo/gamma", branch: "main" }],
      }),
    ]);
    expect(recents.map((r) => r.path)).toEqual(["/repo/alpha"]);
  });
});

describe("mergeRecentProjects", () => {
  it.each([
    [persisted("/repo/frontend", undefined, "frontend"), "frontend"],
    [persisted("/repo/backend"), "backend"],
    [persisted("/"), "/"],
  ])("appends persisted-only %j with a zero count", (entry, displayName) => {
    expect(mergeRecentProjects([], [entry])).toMatchObject([{ displayName, sessionCount: 0 }]);
  });

  it("lets session-derived entries win on a normalized-path collision", () => {
    const live = collectRecentProjects([
      mockSession({ project_path: "/repo/frontend", last_accessed_at: "2025-09-05T00:00:00Z" }),
    ]);
    const merged = mergeRecentProjects(live, [persisted("/repo/frontend/", "2025-01-01T00:00:00+00:00")]);
    expect(merged).toMatchObject([{ sessionCount: 1, lastAccessedAt: "2025-09-05T00:00:00Z" }]);
  });

  it("sorts the combined list newest-first", () => {
    const live = collectRecentProjects([
      mockSession({ project_path: "/repo/live", last_accessed_at: "2025-09-10T00:00:00Z" }),
    ]);
    const merged = mergeRecentProjects(live, [
      persisted("/repo/old", "2025-01-01T00:00:00+00:00"),
      persisted("/repo/recent", "2025-12-01T00:00:00+00:00"),
    ]);
    expect(merged.map((r) => r.path)).toEqual(["/repo/recent", "/repo/live", "/repo/old"]);
  });
});

describe("splitSavedAndRecent", () => {
  const recent = (path: string) => ({ path, displayName: path, lastAccessedAt: null, tool: "claude", sessionCount: 1 });

  it("drops recents that are saved, matching across trailing slashes, and returns saved untouched", () => {
    const savedList = [saved("/repo/alpha/"), saved("/b")];
    const out = splitSavedAndRecent(savedList, [recent("/repo/alpha"), recent("/repo/beta")]);
    expect(out.saved).toBe(savedList);
    expect(out.recent.map((r) => r.path)).toEqual(["/repo/beta"]);
  });
});
