import { describe, it, expect } from "vitest";
import { extractSnippetFromContents } from "./extractSnippetFromContents";

const OLD = "old1\nold2\nold3\nold4\n";
const NEW = "new1\nnew2\nnew3\n";

describe("extractSnippetFromContents", () => {
  it("extracts a new-side range", () => {
    expect(extractSnippetFromContents(OLD, NEW, "new", 1, 2)).toBe(
      "new1\nnew2",
    );
  });

  it("extracts an old-side range", () => {
    expect(extractSnippetFromContents(OLD, NEW, "old", 2, 4)).toBe(
      "old2\nold3\nold4",
    );
  });

  it("extracts a single line", () => {
    expect(extractSnippetFromContents(OLD, NEW, "new", 3, 3)).toBe("new3");
  });

  it("normalizes inverted ranges", () => {
    expect(extractSnippetFromContents(OLD, NEW, "old", 3, 1)).toBe(
      "old1\nold2\nold3",
    );
  });

  it("returns null when the range exceeds the side's line count", () => {
    expect(extractSnippetFromContents(OLD, NEW, "new", 3, 5)).toBeNull();
  });

  it("returns null for a non-positive start", () => {
    expect(extractSnippetFromContents(OLD, NEW, "new", 0, 1)).toBeNull();
  });

  it("handles content without a trailing newline", () => {
    expect(extractSnippetFromContents("a\nb", "", "old", 2, 2)).toBe("b");
  });

  it("returns null on empty content", () => {
    expect(extractSnippetFromContents("", "", "new", 1, 1)).toBeNull();
  });
});
