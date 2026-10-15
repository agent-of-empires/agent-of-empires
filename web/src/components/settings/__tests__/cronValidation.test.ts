import { describe, expect, it } from "vitest";
import { validateCron } from "../cronValidation";

describe("validateCron", () => {
  it("accepts wildcards, lists, ranges, and in-range steps", () => {
    // Both 0 and 7 are Sunday for day-of-week.
    for (const expr of [
      "0 9 * * 1-5",
      "* * * * *",
      "  0   9   *   *   *  ",
      "0,30 9-17 * * *",
      "*/15 * * * *",
      "0 0 1-15/2 * 0",
      "0 0 * * 7",
    ]) {
      expect(validateCron(expr), expr).toBeNull();
    }
  });

  it("rejects the wrong field count, out-of-range values, and malformed items, naming the field", () => {
    const cases: [string, RegExp][] = [
      ["0 9 * *", /exactly 5 fields/],
      ["0 9 * * 1-5 extra", /exactly 5 fields/],
      ["", /exactly 5 fields/],
      ["60 * * * *", /minute/],
      ["* 24 * * *", /hour/],
      ["* * 0 * *", /day-of-month/],
      ["* * * 13 *", /month/],
      ["* * * * 8", /day-of-week/],
      ["*/0 * * * *", /minute/],
      ["*/a * * * *", /minute/],
      ["1-2-3 * * * *", /minute/],
      ["1/2/3 * * * *", /minute/],
      ["abc * * * *", /minute/],
      ["1-99 * * * *", /minute/],
    ];
    for (const [expr, err] of cases) expect(validateCron(expr), expr).toMatch(err);
  });
});
