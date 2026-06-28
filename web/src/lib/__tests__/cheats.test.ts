import { describe, expect, it } from "vitest";
import { CHEATS, matchCheat } from "../cheats";

describe("matchCheat", () => {
  it("matches every registered code exactly", () => {
    for (const code of Object.keys(CHEATS)) {
      expect(matchCheat(code)).toBe(CHEATS[code]);
    }
  });

  it("is case-insensitive and whitespace-tolerant", () => {
    expect(matchCheat("WOLOLO")).toBe(CHEATS["wololo"]);
    expect(matchCheat("  Rock On  ")).toBe(CHEATS["rock on"]);
    expect(matchCheat("how do  you   turn this on")).toBe(CHEATS["how do you turn this on"]);
  });

  it("returns null for ordinary palette searches", () => {
    expect(matchCheat("settings")).toBeNull();
    expect(matchCheat("new session")).toBeNull();
    expect(matchCheat("")).toBeNull();
  });

  it("does not match substrings or prefixes", () => {
    expect(matchCheat("wolol")).toBeNull();
    expect(matchCheat("wololo and more")).toBeNull();
    expect(matchCheat("say marco")).toBeNull();
  });
});
