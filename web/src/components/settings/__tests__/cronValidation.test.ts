// Unit coverage for the client-side 5-field cron validator (#2897). The server
// is authoritative; this only mirrors the grammar for early feedback, so the
// tests pin the field-count, range, list, range, and step branches.

import { describe, expect, it } from "vitest";
import { validateCron } from "../cronValidation";

describe("validateCron", () => {
  it("accepts a plain 5-field expression and wildcards", () => {
    expect(validateCron("0 9 * * 1-5")).toBeNull();
    expect(validateCron("* * * * *")).toBeNull();
    expect(validateCron("  0   9   *   *   *  ")).toBeNull();
  });

  it("accepts lists, ranges, and steps within range", () => {
    expect(validateCron("0,30 9-17 * * *")).toBeNull();
    expect(validateCron("*/15 * * * *")).toBeNull();
    expect(validateCron("0 0 1-15/2 * 0")).toBeNull();
    // Both 0 and 7 are Sunday for day-of-week.
    expect(validateCron("0 0 * * 7")).toBeNull();
  });

  it("rejects the wrong field count", () => {
    expect(validateCron("0 9 * *")).toMatch(/exactly 5 fields/);
    expect(validateCron("0 9 * * 1-5 extra")).toMatch(/exactly 5 fields/);
    expect(validateCron("")).toMatch(/exactly 5 fields/);
  });

  it("rejects out-of-range values per field", () => {
    expect(validateCron("60 * * * *")).toMatch(/minute/);
    expect(validateCron("* 24 * * *")).toMatch(/hour/);
    expect(validateCron("* * 0 * *")).toMatch(/day-of-month/);
    expect(validateCron("* * * 13 *")).toMatch(/month/);
    expect(validateCron("* * * * 8")).toMatch(/day-of-week/);
  });

  it("rejects malformed items (bad step, non-numeric, over-split range)", () => {
    expect(validateCron("*/0 * * * *")).toMatch(/minute/);
    expect(validateCron("*/a * * * *")).toMatch(/minute/);
    expect(validateCron("1-2-3 * * * *")).toMatch(/minute/);
    expect(validateCron("1/2/3 * * * *")).toMatch(/minute/);
    expect(validateCron("abc * * * *")).toMatch(/minute/);
    // Range with an out-of-range endpoint.
    expect(validateCron("1-99 * * * *")).toMatch(/minute/);
  });
});
