// Vitest coverage for `splitSavedAndRecent` (#2140). Saved projects are a
// curated registry surfaced in the wizard's Recent tab alongside the
// session-derived recents. A path present in both sources must render
// once, in the Saved section, so the recent entry is dropped. Path
// matching uses the same trailing-slash normalization as
// `collectRecentProjects` (see ProjectStep.recents-normalize.test.tsx).

import { describe, expect, it } from "vitest";

import { splitSavedAndRecent } from "../steps/ProjectStep";
import type { ProjectInfo } from "../../../lib/types";

function savedProject(overrides: Partial<ProjectInfo> = {}): ProjectInfo {
  return {
    name: overrides.name ?? "alpha",
    path: overrides.path ?? "/repo/alpha",
    scope: overrides.scope ?? "global",
    default_base_branch: overrides.default_base_branch,
  };
}

function recentRow(path: string) {
  return {
    path,
    displayName: path.split("/").filter(Boolean).pop() || path,
    lastAccessedAt: null,
    tool: "claude",
    sessionCount: 1,
  };
}

describe("splitSavedAndRecent (#2140)", () => {
  it("drops a recent whose path is also a saved project, keeping it in Saved only", () => {
    const out = splitSavedAndRecent([savedProject({ path: "/repo/alpha" })], [recentRow("/repo/alpha")]);

    expect(out.saved).toHaveLength(1);
    expect(out.recent).toHaveLength(0);
  });

  it("matches across a trailing-slash difference between the two sources", () => {
    const out = splitSavedAndRecent([savedProject({ path: "/repo/alpha/" })], [recentRow("/repo/alpha")]);

    expect(out.recent).toHaveLength(0);
  });

  it("keeps recents that are not saved projects", () => {
    const out = splitSavedAndRecent([savedProject({ path: "/repo/alpha" })], [recentRow("/repo/beta")]);

    expect(out.saved).toHaveLength(1);
    expect(out.recent.map((r) => r.path)).toEqual(["/repo/beta"]);
  });

  it("returns saved projects untouched", () => {
    const saved = [savedProject({ name: "a", path: "/a" }), savedProject({ name: "b", path: "/b", scope: "profile" })];
    const out = splitSavedAndRecent(saved, []);

    expect(out.saved).toEqual(saved);
    expect(out.recent).toHaveLength(0);
  });
});
