import { describe, it, expect } from "vitest";
import { buildCommentsMarkdown, buildDiffCommentsPrompt, parseDiffCommentsSentinel } from "./buildPrompt";
import type { DiffComment } from "./types";

/** The legacy base64 sentinel encoder that older persisted prompts carry. */
function legacySentinel(payload: object): string {
  const bin = String.fromCharCode(...new TextEncoder().encode(JSON.stringify(payload)));
  return `<!-- aoe:diff-comments:v1 ${btoa(bin)} -->\nbody\n`;
}

function mk(partial: Partial<DiffComment>): DiffComment {
  return {
    id: "c1",
    filePath: "src/foo.rs",
    side: "new",
    startLine: 10,
    endLine: 10,
    body: "body",
    capturedSnippet: "let x = 1;",
    language: "rust",
    createdAt: "2025-01-01T00:00:00Z",
    ...partial,
  };
}

const md = (c: Partial<DiffComment>, isMultiRepo = false) => buildCommentsMarkdown([mk(c)], { isMultiRepo });

describe("buildCommentsMarkdown", () => {
  it.each<[Partial<DiffComment>, boolean, string]>([
    [{ endLine: 14 }, false, "### `src/foo.rs` lines 10-14 (new)"],
    [{}, false, "```rust\nlet x = 1;\n```"],
    [{ repoName: "repoA" }, true, "### [repoA] `src/foo.rs`"],
  ])("renders %j (multi=%s) with %j", (c, multi, expected) => {
    expect(md(c, multi)).toContain(expected);
  });

  it("omits the repo prefix for single-repo and returns empty for no comments", () => {
    expect(md({ repoName: "repoA" })).not.toContain("[repoA]");
    expect(buildCommentsMarkdown([], { isMultiRepo: false })).toBe("");
  });

  it("expands the fence past backticks inside the snippet", () => {
    const out = md({ capturedSnippet: "before\n```\ninner\n```\nafter", language: "" });
    expect(out).toMatch(/^### .*\n\n````\n/m);
    expect(out).toContain("```\ninner\n```");
  });

  it("sorts by repo, file, line, side, createdAt", () => {
    const out = buildCommentsMarkdown(
      [
        mk({ id: "c4", filePath: "src/b.rs", startLine: 5, endLine: 5 }),
        mk({ id: "c1", filePath: "src/a.rs", startLine: 20, endLine: 20 }),
        mk({ id: "c2", filePath: "src/a.rs", startLine: 5, endLine: 5 }),
        mk({ id: "c3", filePath: "src/a.rs", startLine: 5, endLine: 5, side: "old" }),
      ],
      { isMultiRepo: false },
    );
    const order = out
      .split("\n")
      .filter((l) => l.startsWith("### "))
      .map((l) => l.slice(4));
    expect(order).toEqual([
      "`src/a.rs` line 5 (old)",
      "`src/a.rs` line 5 (new)",
      "`src/a.rs` line 20 (new)",
      "`src/b.rs` line 5 (new)",
    ]);
  });
});

describe("buildDiffCommentsPrompt", () => {
  const build = (comments: DiffComment[], intro: string, outro: string, isMultiRepo = false) =>
    buildDiffCommentsPrompt(comments, intro, outro, { isMultiRepo });

  it.each([
    ["", "Please address these comments."],
    ["   outro   ", "outro"],
  ])("outro %j becomes %j and ends the prompt", (outro, expected) => {
    const built = build([mk({})], "", outro);
    expect(built.outro).toBe(expected);
    expect(built.assembledMarkdown.endsWith(`${expected}\n`)).toBe(true);
    if (outro) expect(built.assembledMarkdown).not.toContain("Please address these comments.");
  });

  it("prepends a trimmed intro before the comments section, without a sentinel", () => {
    const built = build([mk({})], "  Hey:  \n", "");
    expect(built.intro).toBe("Hey:");
    expect(built.assembledMarkdown.startsWith("Hey:\n\n## Diff comments")).toBe(true);
    expect(built.assembledMarkdown).not.toContain("<!--");
  });

  it("omits the comments section when there are none", () => {
    const built = build([], "intro", "outro");
    expect(built.assembledMarkdown).toBe("intro\n\noutro\n");
    expect(built.comments).toHaveLength(0);
  });
});

describe("parseDiffCommentsSentinel", () => {
  it("round-trips a legacy payload, including snippets containing -->", () => {
    const comment = mk({ body: "needs error handling", capturedSnippet: "<!-- x -->\nlet x = 1;" });
    const payload = parseDiffCommentsSentinel(
      legacySentinel({ intro: "Take a look:", outro: "Thanks.", isMultiRepo: false, comments: [comment] }),
    );
    expect(payload).toEqual({ intro: "Take a look:", outro: "Thanks.", isMultiRepo: false, comments: [comment] });
  });

  it.each(["hello world", "<!-- aoe:diff-comments:v1 not-base64!@# -->\nbody\n"])("returns null for %j", (text) => {
    expect(parseDiffCommentsSentinel(text)).toBeNull();
  });
});
