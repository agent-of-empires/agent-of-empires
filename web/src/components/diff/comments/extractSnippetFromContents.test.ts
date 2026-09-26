import { describe, it, expect } from "vitest";
import { extractSnippetFromContents } from "./extractSnippetFromContents";

const OLD = "old1\nold2\nold3\nold4\n";
const NEW = "new1\nnew2\nnew3\n";

describe("extractSnippetFromContents", () => {
  it.each<[string, string, "old" | "new", number, number, string | null]>([
    [OLD, NEW, "new", 1, 2, "new1\nnew2"],
    [OLD, NEW, "old", 3, 1, "old1\nold2\nold3"],
    [OLD, NEW, "new", 3, 5, null],
    ["a\nb", "", "old", 2, 2, "b"],
  ])("extracts %#", (oldText, newText, side, start, end, expected) => {
    expect(extractSnippetFromContents(oldText, newText, side, start, end)).toBe(expected);
  });
});
