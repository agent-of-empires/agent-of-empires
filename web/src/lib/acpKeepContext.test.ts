import { describe, expect, it } from "vitest";
import { switchViewCopy } from "./acpKeepContext";

describe("switchViewCopy", () => {
  it("claude keeps context in both directions", () => {
    expect(switchViewCopy(true, true).body).toContain("continues in structured view");
    expect(switchViewCopy(false, true).body).toContain("continues in the terminal");
  });

  it("non-resumable agents restart fresh in both directions", () => {
    expect(switchViewCopy(true, false).body).toContain("fresh conversation");
    expect(switchViewCopy(false, false).body).toContain("restarts in a fresh terminal");
  });

  it("titles reflect the switch direction", () => {
    expect(switchViewCopy(true, true).title).toBe("Switch to structured view");
    expect(switchViewCopy(false, true).title).toBe("Switch to terminal");
  });
});
