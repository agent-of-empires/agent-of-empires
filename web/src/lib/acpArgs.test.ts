import { describe, expect, it } from "vitest";

import {
  hasArgsBody,
  hasTodoArrayArgsText,
  hasTodoItemsArgsText,
  humanizePermissionTitle,
  parseJsonObject,
  pickFirst,
  pickStr,
  previewFromArgs,
  todoItemsFromArgs,
} from "./acpArgs";

it.each([
  ["external_directory", "External directory access"],
  ["Bash", "Bash"],
  ["some_future_kind", "some_future_kind"],
])("humanizePermissionTitle(%j) is %j", (title, expected) => {
  expect(humanizePermissionTitle(title)).toBe(expected);
});

it.each<[string, Record<string, unknown> | null]>([
  ["{}", {}],
  ['{"items":[1,2],"meta":{"n":3}}', { items: [1, 2], meta: { n: 3 } }],
  ...["[]", "42", "null", "not json", "", '{"a":1}[truncated]'].map((input): [string, null] => [input, null]),
])("parseJsonObject(%j)", (input, expected) => {
  expect(parseJsonObject(input)).toEqual(expected);
});

describe("pickStr / pickFirst", () => {
  it("pickStr returns the first string-valued key", () => {
    const o = { command: "ls", path: "/tmp" };
    expect(pickStr(o, "command", "path")).toBe("ls");
    expect(pickStr(o, "path", "command")).toBe("/tmp");
    expect(pickStr({ a: 1, b: true, c: null, d: "found" }, "a", "b", "c", "d")).toBe("found");
    class Bag {
      hidden = "via instance field";
    }
    expect(pickStr(new Bag() as unknown as Record<string, unknown>, "hidden")).toBe("via instance field");
  });

  it.each<[Record<string, unknown> | null, string[]]>([
    [{ a: 1 }, ["b", "c"]],
    [null, ["anything"]],
    [{}, ["a"]],
  ])("pickStr(%j) is null", (o, keys) => {
    expect(pickStr(o, ...keys)).toBeNull();
  });

  it.each<[(string | null | undefined)[], string | null]>([
    [[null, undefined, "", "first", "second"], "first"],
    [["   ", "real"], "real"],
    [[null, undefined, ""], null],
    [[], null],
    [["   ", "\t"], null],
  ])("pickFirst(%j) is %j", (candidates, expected) => {
    expect(pickFirst(...candidates)).toBe(expected);
  });
});

it.each<[string, string | null]>([
  [JSON.stringify({ command: "ls -al" }), "ls -al"],
  [JSON.stringify({ filepath: "/tmp/opencode" }), "/tmp/opencode"],
  [JSON.stringify({ _aoe_title: "Run the suite" }), "Run the suite"],
  ["{}", null],
  [JSON.stringify({ _aoe_parent: "p" }), null],
  ["not json", null],
])("previewFromArgs(%s) is %j", (args, expected) => {
  expect(previewFromArgs(args)).toBe(expected);
});

it.each([
  [JSON.stringify({ command: "ls" }), true],
  ["{}", false],
  [JSON.stringify({ _aoe_title: "x" }), false],
  ["raw text [truncated]", true],
  ["   ", false],
])("hasArgsBody(%j) is %s", (args, expected) => {
  expect(hasArgsBody(args)).toBe(expected);
});

describe("todoItemsFromArgs", () => {
  it("returns todo items with non-blank content, ignoring whitespace-only ones", () => {
    expect(
      todoItemsFromArgs({
        todos: [
          { content: " Check schema ", status: "completed" },
          { content: "   ", status: "pending" },
          { content: "\t", status: "in_progress" },
          { content: "Render todos", status: "in_progress" },
        ],
      }),
    ).toEqual([
      { content: " Check schema ", status: "completed" },
      { content: "Render todos", status: "in_progress" },
    ]);
  });

  it("detects todo args only when at least one item has content", () => {
    expect(hasTodoItemsArgsText(JSON.stringify({ todos: [{ content: "   ", status: "pending" }] }))).toBe(false);
    expect(hasTodoItemsArgsText(JSON.stringify({ todos: [{ content: "Real", status: "pending" }] }))).toBe(true);
  });
});

it.each([
  [JSON.stringify({ todos: [] }), true],
  [JSON.stringify({ todos: [{ content: "Real", status: "pending" }] }), true],
  [JSON.stringify({ thought: "thinking" }), false],
  [JSON.stringify({ todos: "nope" }), false],
  ["not json", false],
])("hasTodoArrayArgsText(%s) is %s", (args, expected) => {
  expect(hasTodoArrayArgsText(args)).toBe(expected);
});
