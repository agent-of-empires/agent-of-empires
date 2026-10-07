import { expect, it } from "vitest";
import { diffSelectionStale } from "../diffSelection";
import type { RichDiffFile } from "../types";

const file = (path: string, repo_name?: string): RichDiffFile => ({
  path,
  old_path: null,
  status: "modified",
  additions: 0,
  deletions: 0,
  repo_name,
});

it("diffSelectionStale only for a loaded, uncited selection missing from the diff", () => {
  const cases: [string, Parameters<typeof diffSelectionStale>, boolean][] = [
    ["no selection", [null, false, [file("a.ts")]], false],
    ["cited", [{ path: "b.ts", cited: true }, false, [file("a.ts")]], false],
    ["loading", [{ path: "b.ts" }, true, []], false],
    ["absent", [{ path: "b.ts" }, false, [file("a.ts")]], true],
    ["present", [{ path: "a.ts", repoName: "api" }, false, [file("a.ts", "api")]], false],
    ["other repo", [{ path: "a.ts", repoName: "web" }, false, [file("a.ts", "api")]], true],
  ];
  for (const [name, args, expected] of cases) expect(diffSelectionStale(...args), name).toBe(expected);
});
