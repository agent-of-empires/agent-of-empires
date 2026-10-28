import { describe, expect, it } from "vitest";

import { checklistMarkdown } from "./checklistMarkdown";

describe("checklistMarkdown", () => {
  it("renders every state as a valid GFM task item", () => {
    expect(
      checklistMarkdown([
        { text: "one", status: "done" },
        { text: "two", status: "in_progress" },
        { text: "three", status: "pending" },
        { text: "four", status: "cancelled" },
      ]),
    ).toBe("- [x] one\n- [ ] two (in progress)\n- [ ] three\n- [ ] ~~four~~");
  });

  it("is empty for no items", () => {
    expect(checklistMarkdown([])).toBe("");
  });
});
