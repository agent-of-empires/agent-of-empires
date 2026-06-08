// Type-level regression tests for the structured view wire types. These run
// under Vitest's typecheck mode (see `test.typecheck` in
// `vite.config.ts`); a failing assertion is a tsc error, not a runtime
// failure. Guards #1562: the Rust `ConfigOptionCategory::Other(String)`
// arm is `#[serde(untagged)]`, so an unknown category arrives on the
// wire as a bare string. The TS type must accept any string for those,
// not the `{ Other: string }` object shape that never matches.

import { describe, expectTypeOf, it } from "vitest";

import type { ConfigOptionCategory } from "../acpTypes";

describe("ConfigOptionCategory", () => {
  it("accepts an arbitrary wire string for unknown categories", () => {
    // Fails to type-check under the old `{ Other: string }` variant: a
    // bare string is not assignable to it.
    expectTypeOf<string>().toExtend<ConfigOptionCategory>();
    expectTypeOf("future_category").toExtend<ConfigOptionCategory>();
  });

  it("still admits the known spec literals", () => {
    expectTypeOf<"mode">().toExtend<ConfigOptionCategory>();
    expectTypeOf<"model">().toExtend<ConfigOptionCategory>();
    expectTypeOf<"thought_level">().toExtend<ConfigOptionCategory>();
  });
});
