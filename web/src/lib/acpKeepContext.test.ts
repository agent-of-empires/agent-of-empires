import { describe, expect, it } from "vitest";
import { switchViewCopy } from "./acpKeepContext";

describe("switchViewCopy", () => {
  it("non-resumable agents restart fresh in both directions", () => {
    expect(switchViewCopy(true, false).body).toContain("fresh conversation");
    expect(switchViewCopy(false, false).body).toContain("restarts in a fresh terminal");
  });

  it("resumable agents qualify the continuity promise in both directions", () => {
    expect(switchViewCopy(true, true).body).toContain("when it has a saved transcript, otherwise it starts fresh");
    expect(switchViewCopy(false, true).body).toContain(
      "when it can be resumed with `--resume`, otherwise the agent starts fresh",
    );
  });
});
