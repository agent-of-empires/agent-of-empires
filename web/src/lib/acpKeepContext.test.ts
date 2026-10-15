import { describe, expect, it } from "vitest";
import { switchViewCopy } from "./acpKeepContext";

describe("switchViewCopy", () => {
  it("non-resumable agents restart fresh in both directions", () => {
    expect(switchViewCopy(true, false).body).toContain("fresh conversation");
    expect(switchViewCopy(false, false).body).toContain("restarts in a fresh terminal");
  });
});
