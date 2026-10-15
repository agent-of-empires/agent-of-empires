import { describe, expect, it } from "vitest";
import {
  parseFileRef,
  relativeDisplayPath,
  resolveArtifactUrl,
  resolveToRepoRelative,
  type FileRefSession,
} from "../fileRef";

const session = (over: Partial<FileRefSession> = {}): FileRefSession => ({
  id: "s1",
  project_path: "/Users/me/.aoe/worktrees/feat",
  main_repo_path: "/Users/me/repo",
  workspace_repos: [],
  ...over,
});
const workspace = session({
  project_path: "/Users/me/.aoe/worktrees/ws",
  main_repo_path: null,
  workspace_repos: [
    { name: "api", source_path: "/Users/me/api" },
    { name: "web", source_path: "/Users/me/web" },
  ],
});

it.each<[string, string | null]>([
  ["/aoe/artifacts/shot.png", "/api/sessions/sess-1/artifacts/shot.png"],
  ["/Users/me/.aoe/artifacts/sess-1/sub/x.png", "/api/sessions/sess-1/artifacts/sub/x.png"],
  ["/aoe/artifacts/a b/c#d.png", "/api/sessions/sess-1/artifacts/a%20b/c%23d.png"],
  ["/aoe/artifacts/./sub//x.png", "/api/sessions/sess-1/artifacts/sub/x.png"],
  ["/tmp/elsewhere/x.png", null],
  ["/Users/me/repo/src/app.ts", null],
  ["/aoe/artifacts", null],
  ["/aoe/artifacts/../../etc/passwd", null],
  ["/aoe/artifacts/sub/../x.png", null],
])("resolveArtifactUrl(%j) is %j (#2587)", (path, expected) => {
  const s = session({ id: "sess-1", artifact_dir: "/Users/me/.aoe/artifacts/sess-1" });
  expect(resolveArtifactUrl(path, s)).toBe(expected);
});

describe("parseFileRef", () => {
  it.each<[string, ReturnType<typeof parseFileRef>]>([
    ["/Users/me/repo/src/app.ts:42", { path: "/Users/me/repo/src/app.ts", line: 42 }],
    ["/repo/src/app.ts:42:7", { path: "/repo/src/app.ts", line: 42, column: 7 }],
    ["/repo/src/app.ts#L99", { path: "/repo/src/app.ts", line: 99 }],
    ["/repo/src/app.ts", { path: "/repo/src/app.ts" }],
    ["src/app.ts:10", { path: "src/app.ts", line: 10 }],
    ["C:\\repo\\src\\app.ts:5", { path: "C:/repo/src/app.ts", line: 5 }],
    ["C:\\repo\\app.ts", { path: "C:/repo/app.ts" }],
    ["file:///Users/me/repo/app.ts:3", { path: "/Users/me/repo/app.ts", line: 3 }],
    ["file:///C:/repo/app.ts", { path: "C:/repo/app.ts" }],
    ["/repo/my%20dir/app.ts:1", { path: "/repo/my dir/app.ts", line: 1 }],
    ["/repo/a%ZZb.ts", { path: "/repo/a%ZZb.ts" }],
  ])("parses %j", (href, expected) => {
    expect(parseFileRef(href)).toEqual(expected);
  });

  it.each([
    "https://example.com/a.ts",
    "http://example.com",
    "mailto:me@example.com",
    "tel:+15551234",
    "data:text/plain,hi",
    "javascript:alert(1)",
    "vscode://file/x",
    "//cdn.example.com/x.js",
    "#section",
    "",
    "   ",
  ])("returns null for non-file href %j", (href) => {
    expect(parseFileRef(href)).toBeNull();
  });
});

it.each<[string, string, FileRefSession, ReturnType<typeof resolveToRepoRelative>]>([
  ["the worktree", "/Users/me/.aoe/worktrees/feat/src/app.ts", session(), { relativePath: "src/app.ts" }],
  ["the main repo", "/Users/me/repo/src/app.ts", session(), { relativePath: "src/app.ts" }],
  ["a prefix-sharing sibling", "/Users/me/repo_old/src/app.ts", session(), null],
  ["no known root", "/etc/passwd", session(), null],
  ["a relative path", "src/app.ts", session(), { relativePath: "src/app.ts" }],
  ["a ./ relative path", "./src/app.ts", session(), { relativePath: "src/app.ts" }],
  [
    "a case-folded Windows drive",
    "c:\\Users\\me\\repo\\src\\app.ts",
    session({ project_path: "C:\\Users\\me\\repo", main_repo_path: null }),
    { relativePath: "src/app.ts" },
  ],
  ["a workspace repo", "/Users/me/web/src/app.ts", workspace, { relativePath: "src/app.ts", repoName: "web" }],
])("resolveToRepoRelative against %s", (_name, path, s, expected) => {
  expect(resolveToRepoRelative(path, s)).toEqual(expected);
});

it.each<[string, FileRefSession | null, string]>([
  ["/Users/me/.aoe/worktrees/feat/src/hooks/mod.rs", session(), "src/hooks/mod.rs"],
  ["/Users/me/api/src/h.ts", workspace, "api/src/h.ts"],
  ["/etc/hosts", session(), "/etc/hosts"],
  ["/Users/me/.aoe/worktrees/feat/src/app.ts", null, "/Users/me/.aoe/worktrees/feat/src/app.ts"],
])("relativeDisplayPath(%j) is %j", (path, s, expected) => {
  expect(relativeDisplayPath(path, s)).toBe(expected);
});
