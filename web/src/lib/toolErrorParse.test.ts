import { expect, it } from "vitest";

import { describeToolErrorTag, parseToolError } from "./toolErrorParse";

it("parseToolError strips a matched wrapper and reports its tag", () => {
  const long =
    "File does not exist. Note: your current working directory is /Users/seluj78/aoe/dev-agent-of-empires-worktrees/test31.";
  const cases: [string | null | undefined, string, string | null][] = [
    [undefined, "", null],
    [null, "", null],
    ["   ", "", null],
    ["<tool_use_error>File has not been read yet</tool_use_error>", "File has not been read yet", "tool_use_error"],
    ["<error>Something broke</error>", "Something broke", "error"],
    ["\n  <tool_use_error>nope</tool_use_error>  \n", "nope", "tool_use_error"],
    ["file not found: foo.rs", "file not found: foo.rs", null],
    ["<tool_use_error>oops</different_tag>", "<tool_use_error>oops</different_tag>", null],
    ["<tool_use_error>line one\nline two</tool_use_error>", "line one\nline two", "tool_use_error"],
    ["Preamble note\n<tool_use_error>File does not exist.</tool_use_error>", "File does not exist.", "tool_use_error"],
    ["<tool_use_error>File does not exist.</tool_use_error>\n```\n```", "File does not exist.", "tool_use_error"],
    [`<tool_use_error>${long}</tool_use_error>`, long, "tool_use_error"],
  ];
  for (const [raw, body, tag] of cases) expect(parseToolError(raw), String(raw)).toEqual({ body, tag });
});

it("describeToolErrorTag labels known wrappers and passes others through", () => {
  expect(describeToolErrorTag(null)).toBeNull();
  expect(describeToolErrorTag("tool_use_error")).toBe("agent-reported error");
  expect(describeToolErrorTag("tool_result_error")).toBe("agent-reported error");
  expect(describeToolErrorTag("error")).toBe("error");
  expect(describeToolErrorTag("custom_wrapper")).toBe("custom_wrapper");
});
