// Tests for the `<tool_use_error>` wrapper-parser used by every per-kind
// structured view tool card's error body. See issue #1090.

import { describe, expect, it } from "vitest";

import { describeToolErrorTag, parseToolError } from "./toolErrorParse";

describe("parseToolError", () => {
  it("returns empty body when the result text is missing", () => {
    expect(parseToolError(undefined)).toEqual({ body: "", tag: null });
    expect(parseToolError(null)).toEqual({ body: "", tag: null });
    expect(parseToolError("")).toEqual({ body: "", tag: null });
    expect(parseToolError("   ")).toEqual({ body: "", tag: null });
  });

  it("strips the <tool_use_error> wrapper and reports the tag", () => {
    expect(parseToolError("<tool_use_error>File has not been read yet</tool_use_error>")).toEqual({
      body: "File has not been read yet",
      tag: "tool_use_error",
    });
  });

  it("strips arbitrary single-pair tags and surfaces the name", () => {
    expect(parseToolError("<error>Something broke</error>")).toEqual({
      body: "Something broke",
      tag: "error",
    });
  });

  it("tolerates leading/trailing whitespace around the wrapper", () => {
    expect(parseToolError("\n  <tool_use_error>nope</tool_use_error>  \n")).toEqual({
      body: "nope",
      tag: "tool_use_error",
    });
  });

  it("returns the raw text as body when there is no wrapper", () => {
    expect(parseToolError("file not found: foo.rs")).toEqual({
      body: "file not found: foo.rs",
      tag: null,
    });
  });

  it("does not match mismatched open/close tags", () => {
    expect(parseToolError("<tool_use_error>oops</different_tag>")).toEqual({
      body: "<tool_use_error>oops</different_tag>",
      tag: null,
    });
  });

  it("preserves inner-text linebreaks", () => {
    const raw = "<tool_use_error>line one\nline two\nline three</tool_use_error>";
    const parsed = parseToolError(raw);
    expect(parsed.tag).toBe("tool_use_error");
    expect(parsed.body).toBe("line one\nline two\nline three");
  });

  it("strips the wrapper when prose precedes it", () => {
    // claude-agent-acp sometimes joins multiple ContentBlock::Text
    // entries with `\n` before the wrapper; the anchored regex used to
    // miss this and leak `<tool_use_error>…</tool_use_error>` into the
    // rendered body. See follow-up to #1090.
    const raw = "Preamble note\n<tool_use_error>File does not exist.</tool_use_error>";
    expect(parseToolError(raw)).toEqual({
      body: "File does not exist.",
      tag: "tool_use_error",
    });
  });

  it("strips trailing empty code fences glued onto the wrapper", () => {
    // Observed in the wild: claude-agent-acp emits a second
    // ContentBlock::Text containing an empty markdown code fence after
    // the wrapper. `extract_tool_content_text` joins blocks with `\n`,
    // so the body used to render an empty `` ``` ``` `` below the
    // unwrapped error. Drop adapter formatting noise entirely.
    const raw = "<tool_use_error>File does not exist.</tool_use_error>\n```\n```";
    expect(parseToolError(raw)).toEqual({
      body: "File does not exist.",
      tag: "tool_use_error",
    });
  });

  it("strips the wrapper from a long path-bearing message", () => {
    // Regression for the reported case: a `Read` of a missing file
    // returns this exact shape from claude-agent-acp.
    const raw =
      "<tool_use_error>File does not exist. Note: your current working directory is /Users/seluj78/aoe/dev-agent-of-empires-worktrees/test31.</tool_use_error>";
    const parsed = parseToolError(raw);
    expect(parsed.tag).toBe("tool_use_error");
    expect(parsed.body).toBe(
      "File does not exist. Note: your current working directory is /Users/seluj78/aoe/dev-agent-of-empires-worktrees/test31.",
    );
  });
});

describe("describeToolErrorTag", () => {
  it("returns null for a missing tag", () => {
    expect(describeToolErrorTag(null)).toBeNull();
  });

  it("maps known agent wrappers to a friendly label", () => {
    expect(describeToolErrorTag("tool_use_error")).toBe("agent-reported error");
    expect(describeToolErrorTag("tool_result_error")).toBe("agent-reported error");
    expect(describeToolErrorTag("error")).toBe("error");
  });

  it("passes unknown tags through verbatim", () => {
    expect(describeToolErrorTag("custom_wrapper")).toBe("custom_wrapper");
  });
});
