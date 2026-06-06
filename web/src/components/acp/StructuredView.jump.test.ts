// @vitest-environment jsdom

import { describe, expect, it, vi } from "vitest";

import { scrollLatestAssistantMessageIntoView } from "./scrollLatestAssistantMessageIntoView";

describe("scrollLatestAssistantMessageIntoView", () => {
  it("scrolls the latest assistant message to the top of the viewport", () => {
    const viewport = document.createElement("div");
    const first = document.createElement("div");
    const latest = document.createElement("div");
    first.dataset.acpMessageRole = "assistant";
    latest.dataset.acpMessageRole = "assistant";
    first.scrollIntoView = vi.fn();
    latest.scrollIntoView = vi.fn();
    viewport.append(first, latest);

    expect(scrollLatestAssistantMessageIntoView(viewport)).toBe(true);
    expect(first.scrollIntoView).not.toHaveBeenCalled();
    expect(latest.scrollIntoView).toHaveBeenCalledWith({
      block: "start",
      behavior: "smooth",
    });
  });

  it("returns false when there is no assistant message", () => {
    const viewport = document.createElement("div");

    expect(scrollLatestAssistantMessageIntoView(viewport)).toBe(false);
  });
});
