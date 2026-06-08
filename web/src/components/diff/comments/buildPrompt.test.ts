import { describe, it, expect } from "vitest";
import {
  buildCommentsMarkdown,
  buildDiffCommentsPrompt,
  parseDiffCommentsSentinel,
  stripDiffCommentsSentinel,
} from "./buildPrompt";
import type { DiffComment } from "./types";

/** Reproduce the legacy `<!-- aoe:diff-comments:v1 <base64> -->`
 *  encoder (removed from the send path in #1123) so the decode-fallback
 *  tests can still exercise `parseDiffCommentsSentinel` against the
 *  shape older persisted prompts carry. */
function legacySentinel(payload: {
  intro: string;
  outro: string;
  isMultiRepo: boolean;
  comments: DiffComment[];
}): string {
  const json = JSON.stringify(payload);
  const bytes = new TextEncoder().encode(json);
  let bin = "";
  for (let i = 0; i < bytes.length; i++) bin += String.fromCharCode(bytes[i]!);
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

describe("buildCommentsMarkdown", () => {
  it("renders single-line wording", () => {
    const md = buildCommentsMarkdown([mk({})], { isMultiRepo: false });
    expect(md).toContain("### `src/foo.rs` line 10 (new)");
  });

  it("renders range wording", () => {
    const md = buildCommentsMarkdown([mk({ startLine: 10, endLine: 14 })], {
      isMultiRepo: false,
    });
    expect(md).toContain("### `src/foo.rs` lines 10-14 (new)");
  });

  it("includes a code fence with language", () => {
    const md = buildCommentsMarkdown([mk({})], { isMultiRepo: false });
    expect(md).toContain("```rust\nlet x = 1;\n```");
  });

  it("expands the fence when snippet contains backticks", () => {
    const md = buildCommentsMarkdown(
      [
        mk({
          capturedSnippet: "before\n```\ninner\n```\nafter",
          language: "",
        }),
      ],
      { isMultiRepo: false },
    );
    expect(md).toMatch(/^### .*\n\n````\n/m);
    expect(md).toContain("```\ninner\n```");
  });

  it("prefixes repo when isMultiRepo", () => {
    const md = buildCommentsMarkdown([mk({ repoName: "repoA" })], {
      isMultiRepo: true,
    });
    expect(md).toContain("### [repoA] `src/foo.rs`");
  });

  it("does not prefix repo when not multi-repo even if repoName set", () => {
    const md = buildCommentsMarkdown([mk({ repoName: "repoA" })], {
      isMultiRepo: false,
    });
    expect(md).not.toContain("[repoA]");
  });

  it("sorts by repo, file, line, side, createdAt", () => {
    const md = buildCommentsMarkdown(
      [
        mk({ id: "c4", filePath: "src/b.rs", startLine: 5, endLine: 5 }),
        mk({ id: "c1", filePath: "src/a.rs", startLine: 20, endLine: 20 }),
        mk({ id: "c2", filePath: "src/a.rs", startLine: 5, endLine: 5 }),
        mk({
          id: "c3",
          filePath: "src/a.rs",
          startLine: 5,
          endLine: 5,
          side: "old",
        }),
      ],
      { isMultiRepo: false },
    );
    const order = md
      .split("\n")
      .filter((l) => l.startsWith("### "))
      .map((l) => l.slice("### ".length));
    expect(order).toEqual([
      "`src/a.rs` line 5 (old)",
      "`src/a.rs` line 5 (new)",
      "`src/a.rs` line 20 (new)",
      "`src/b.rs` line 5 (new)",
    ]);
  });

  it("returns empty string for no comments", () => {
    expect(buildCommentsMarkdown([], { isMultiRepo: false })).toBe("");
  });
});

describe("buildDiffCommentsPrompt", () => {
  it("uses default outro when blank", () => {
    const built = buildDiffCommentsPrompt([mk({})], "", "", {
      isMultiRepo: false,
    });
    expect(built.outro).toBe("Please address these comments.");
    expect(built.assembledMarkdown).toMatch(
      /Please address these comments\.\n$/,
    );
  });

  it("uses provided outro when set", () => {
    const built = buildDiffCommentsPrompt([mk({})], "", "Custom outro", {
      isMultiRepo: false,
    });
    expect(built.outro).toBe("Custom outro");
    expect(built.assembledMarkdown).toMatch(/Custom outro\n$/);
    expect(built.assembledMarkdown).not.toContain(
      "Please address these comments.",
    );
  });

  it("prepends intro when set", () => {
    const built = buildDiffCommentsPrompt([mk({})], "Hey:", "", {
      isMultiRepo: false,
    });
    expect(built.intro).toBe("Hey:");
    expect(built.assembledMarkdown.startsWith("Hey:\n\n## Diff comments")).toBe(
      true,
    );
  });

  it("trims surrounding whitespace from intro/outro", () => {
    const built = buildDiffCommentsPrompt(
      [mk({})],
      "  intro  \n",
      "   outro   ",
      {
        isMultiRepo: false,
      },
    );
    expect(built.intro).toBe("intro");
    expect(built.outro).toBe("outro");
    expect(built.assembledMarkdown.startsWith("intro\n")).toBe(true);
    expect(built.assembledMarkdown).toMatch(/outro\n$/);
  });

  it("omits the comments section when no comments", () => {
    const built = buildDiffCommentsPrompt([], "intro", "outro", {
      isMultiRepo: false,
    });
    expect(built.assembledMarkdown).not.toContain("## Diff comments");
    expect(built.assembledMarkdown).toContain("intro");
    expect(built.assembledMarkdown).toContain("outro");
    expect(built.comments).toHaveLength(0);
  });

  it("never emits a sentinel header (typed-event send path)", () => {
    const built = buildDiffCommentsPrompt([mk({})], "", "", {
      isMultiRepo: false,
    });
    expect(built.assembledMarkdown).not.toContain("aoe:diff-comments");
    expect(built.assembledMarkdown).not.toContain("<!--");
    expect(built.assembledMarkdown).toContain("## Diff comments");
  });

  it("carries the structured comments and multi-repo flag verbatim", () => {
    const comment = mk({ repoName: "repoA" });
    const built = buildDiffCommentsPrompt([comment], "", "", {
      isMultiRepo: true,
    });
    expect(built.isMultiRepo).toBe(true);
    expect(built.comments).toEqual([comment]);
  });
});

describe("parseDiffCommentsSentinel (legacy decode fallback)", () => {
  it("returns null when no sentinel", () => {
    expect(parseDiffCommentsSentinel("hello world")).toBeNull();
  });

  it("round-trips a legacy sentinel payload", () => {
    const comment = mk({
      body: "needs error handling",
      capturedSnippet: "let x = 1;",
    });
    const prompt = legacySentinel({
      intro: "Take a look:",
      outro: "Thanks.",
      isMultiRepo: false,
      comments: [comment],
    });
    const payload = parseDiffCommentsSentinel(prompt);
    expect(payload).not.toBeNull();
    expect(payload!.intro).toBe("Take a look:");
    expect(payload!.outro).toBe("Thanks.");
    expect(payload!.isMultiRepo).toBe(false);
    expect(payload!.comments).toHaveLength(1);
    expect(payload!.comments[0]!.body).toBe("needs error handling");
    expect(payload!.comments[0]!.capturedSnippet).toBe("let x = 1;");
  });

  it("survives snippets that contain `-->`", () => {
    const c = mk({ capturedSnippet: "<!-- not allowed -->\nlet x = 1;" });
    const prompt = legacySentinel({
      intro: "",
      outro: "Please address these comments.",
      isMultiRepo: false,
      comments: [c],
    });
    const payload = parseDiffCommentsSentinel(prompt);
    expect(payload).not.toBeNull();
    expect(payload!.comments[0]!.capturedSnippet).toContain("-->");
  });

  it("returns null for a malformed payload", () => {
    const broken = "<!-- aoe:diff-comments:v1 not-base64!@# -->\nbody\n";
    expect(parseDiffCommentsSentinel(broken)).toBeNull();
  });

  it("strips the sentinel to recover the visible body", () => {
    const prompt = legacySentinel({
      intro: "Intro:",
      outro: "Outro.",
      isMultiRepo: false,
      comments: [mk({})],
    });
    const body = stripDiffCommentsSentinel(prompt);
    expect(body).toBe("body\n");
  });
});
