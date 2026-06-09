import { describe, expect, it } from "vitest";
import { parseFileRef, resolveToRepoRelative, type FileRefSession } from "../fileRef";

describe("parseFileRef", () => {
  it("parses an absolute path with a line suffix", () => {
    expect(parseFileRef("/Users/me/repo/src/app.ts:42")).toEqual({
      path: "/Users/me/repo/src/app.ts",
      line: 42,
    });
  });

  it("parses a line:column suffix", () => {
    expect(parseFileRef("/repo/src/app.ts:42:7")).toEqual({
      path: "/repo/src/app.ts",
      line: 42,
      column: 7,
    });
  });

  it("parses the #Lline (GitHub blob) suffix", () => {
    expect(parseFileRef("/repo/src/app.ts#L99")).toEqual({
      path: "/repo/src/app.ts",
      line: 99,
    });
  });

  it("parses an absolute path with no suffix", () => {
    expect(parseFileRef("/repo/src/app.ts")).toEqual({
      path: "/repo/src/app.ts",
    });
  });

  it("treats a relative path as a file reference", () => {
    expect(parseFileRef("src/app.ts:10")).toEqual({
      path: "src/app.ts",
      line: 10,
    });
  });

  it("normalizes Windows backslashes and keeps the drive letter", () => {
    expect(parseFileRef("C:\\repo\\src\\app.ts:5")).toEqual({
      path: "C:/repo/src/app.ts",
      line: 5,
    });
  });

  it("does not eat a bare Windows drive colon", () => {
    // No numeric line here, so nothing should be stripped.
    expect(parseFileRef("C:\\repo\\app.ts")).toEqual({
      path: "C:/repo/app.ts",
    });
  });

  it("strips a file:// scheme", () => {
    expect(parseFileRef("file:///Users/me/repo/app.ts:3")).toEqual({
      path: "/Users/me/repo/app.ts",
      line: 3,
    });
  });

  it("strips a Windows file:// scheme to a drive-led path", () => {
    expect(parseFileRef("file:///C:/repo/app.ts")).toEqual({
      path: "C:/repo/app.ts",
    });
  });

  it("decodes percent-encoded characters (spaces)", () => {
    expect(parseFileRef("/repo/my%20dir/app.ts:1")).toEqual({
      path: "/repo/my dir/app.ts",
      line: 1,
    });
  });

  it("survives malformed percent-encoding", () => {
    expect(parseFileRef("/repo/a%ZZb.ts")).toEqual({ path: "/repo/a%ZZb.ts" });
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
  ])("returns null for non-file href %s", (href) => {
    expect(parseFileRef(href)).toBeNull();
  });

  it("returns null for empty/whitespace href", () => {
    expect(parseFileRef("")).toBeNull();
    expect(parseFileRef("   ")).toBeNull();
  });
});

describe("resolveToRepoRelative", () => {
  const single: FileRefSession = {
    project_path: "/Users/me/.aoe/worktrees/feat",
    main_repo_path: "/Users/me/repo",
    workspace_repos: [],
  };

  it("resolves a path inside the worktree to a repo-relative path", () => {
    expect(resolveToRepoRelative("/Users/me/.aoe/worktrees/feat/src/app.ts", single)).toEqual({
      relativePath: "src/app.ts",
    });
  });

  it("falls back to the main repo path", () => {
    expect(resolveToRepoRelative("/Users/me/repo/src/app.ts", single)).toEqual({
      relativePath: "src/app.ts",
    });
  });

  it("does not match a sibling dir with a shared prefix", () => {
    // `/Users/me/repo` must not match `/Users/me/repo_old/...`.
    expect(resolveToRepoRelative("/Users/me/repo_old/src/app.ts", single)).toBeNull();
  });

  it("returns null when the path is outside any known root", () => {
    expect(resolveToRepoRelative("/etc/passwd", single)).toBeNull();
  });

  it("treats a relative path as already repo-relative", () => {
    expect(resolveToRepoRelative("src/app.ts", single)).toEqual({
      relativePath: "src/app.ts",
    });
  });

  it("strips a leading ./ from a relative path", () => {
    expect(resolveToRepoRelative("./src/app.ts", single)).toEqual({
      relativePath: "src/app.ts",
    });
  });

  it("matches a Windows drive root case-insensitively", () => {
    const win: FileRefSession = {
      project_path: "C:\\Users\\me\\repo",
      main_repo_path: null,
      workspace_repos: [],
    };
    expect(resolveToRepoRelative("c:\\Users\\me\\repo\\src\\app.ts", win)).toEqual({ relativePath: "src/app.ts" });
  });

  it("resolves against a workspace repo root and returns its name", () => {
    const workspace: FileRefSession = {
      project_path: "/Users/me/.aoe/worktrees/ws",
      main_repo_path: null,
      workspace_repos: [
        { name: "api", source_path: "/Users/me/api" },
        { name: "web", source_path: "/Users/me/web" },
      ],
    };
    expect(resolveToRepoRelative("/Users/me/web/src/app.ts", workspace)).toEqual({
      relativePath: "src/app.ts",
      repoName: "web",
    });
  });
});
