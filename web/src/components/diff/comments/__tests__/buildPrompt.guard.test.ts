// @vitest-environment node

import { describe, expect, it } from "vitest";

import { isDiffCommentsCardPayload } from "../buildPrompt";

describe("isDiffCommentsCardPayload", () => {
  it("accepts a well-formed payload", () => {
    expect(
      isDiffCommentsCardPayload({
        intro: "intro",
        outro: "outro",
        isMultiRepo: false,
        comments: [],
      }),
    ).toBe(true);
  });

  it("rejects non-objects", () => {
    expect(isDiffCommentsCardPayload(undefined)).toBe(false);
    expect(isDiffCommentsCardPayload(null)).toBe(false);
    expect(isDiffCommentsCardPayload("nope")).toBe(false);
    expect(isDiffCommentsCardPayload(42)).toBe(false);
  });

  it("rejects payloads with a non-array comments field", () => {
    expect(
      isDiffCommentsCardPayload({
        intro: "intro",
        outro: "outro",
        isMultiRepo: false,
        comments: "not-an-array",
      }),
    ).toBe(false);
  });

  it("rejects payloads missing required string fields", () => {
    expect(isDiffCommentsCardPayload({ isMultiRepo: false, comments: [] })).toBe(false);
    expect(
      isDiffCommentsCardPayload({
        intro: "intro",
        outro: "outro",
        isMultiRepo: "yes",
        comments: [],
      }),
    ).toBe(false);
  });
});
