// @vitest-environment node

import { describe, expect, it } from "vitest";

import { isDiffCommentsCardPayload } from "../buildPrompt";

describe("isDiffCommentsCardPayload", () => {
  it("accepts a well-formed payload", () => {
    expect(isDiffCommentsCardPayload({ intro: "intro", outro: "outro", isMultiRepo: false, comments: [] })).toBe(true);
  });

  it("rejects non-objects, non-array comments, and missing or mistyped fields", () => {
    for (const value of [
      undefined,
      null,
      "nope",
      42,
      { intro: "intro", outro: "outro", isMultiRepo: false, comments: "not-an-array" },
      { isMultiRepo: false, comments: [] },
      { intro: "intro", outro: "outro", isMultiRepo: "yes", comments: [] },
    ]) {
      expect(isDiffCommentsCardPayload(value), JSON.stringify(value)).toBe(false);
    }
  });
});
