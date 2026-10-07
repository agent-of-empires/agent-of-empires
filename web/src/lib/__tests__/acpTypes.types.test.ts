import { describe, expectTypeOf, it } from "vitest";

import type { ConfigOptionCategory } from "../acpTypes";

describe("ConfigOptionCategory", () => {
  it("accepts an arbitrary wire string for unknown categories", () => {
    expectTypeOf<string>().toExtend<ConfigOptionCategory>();
    expectTypeOf("future_category").toExtend<ConfigOptionCategory>();
  });

  it("still admits the known spec literals", () => {
    expectTypeOf<"mode">().toExtend<ConfigOptionCategory>();
    expectTypeOf<"model">().toExtend<ConfigOptionCategory>();
    expectTypeOf<"thought_level">().toExtend<ConfigOptionCategory>();
  });
});
