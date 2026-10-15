// @vitest-environment jsdom

import { afterEach, describe, expect, it } from "vitest";
import { cleanup, render, screen } from "@testing-library/react";

import { CreateProgressView } from "../CreateProgressView";
import type { CreateProgress } from "../../../lib/types";

afterEach(cleanup);

// jsdom has no layout; scrollHeight grows with the text, like a wrapping log.
Object.defineProperty(HTMLElement.prototype, "scrollHeight", {
  configurable: true,
  get(this: HTMLElement) {
    return this.textContent?.length ?? 0;
  },
});

const snapshot = (tail: string): CreateProgress => ({
  stage: "running_hooks",
  hook: "npm ci",
  // A full window: the server keeps the newest 200 lines.
  output: [...Array.from({ length: 199 }, (_, i) => `line ${i}`), tail],
});

describe("CreateProgressView", () => {
  it("follows a saturated window whose new tail wraps", () => {
    const { rerender } = render(<CreateProgressView progress={snapshot("short")} />);
    const out = screen.getByTestId("create-progress-output");
    const first = out.scrollTop;
    rerender(<CreateProgressView progress={snapshot("x".repeat(400))} />);
    expect(out.scrollTop).toBeGreaterThan(first);
    expect(out.scrollTop).toBe(out.scrollHeight);
  });
});
