// @vitest-environment jsdom
import { describe, expect, it } from "vitest";
import { shouldFocusComposerOnThreadTap } from "./threadTapFocus";

function el(html: string): Element {
  const host = document.createElement("div");
  host.innerHTML = html;
  return host.firstElementChild!;
}

describe("shouldFocusComposerOnThreadTap", () => {
  const plain = el("<p>hello</p>");

  it("focuses on a coarse-pointer tap on plain transcript text", () => {
    expect(shouldFocusComposerOnThreadTap({ isCoarse: true, target: plain, hasSelection: false })).toBe(true);
  });

  it("never fires on a fine pointer (desktop already auto-focuses the composer)", () => {
    expect(shouldFocusComposerOnThreadTap({ isCoarse: false, target: plain, hasSelection: false })).toBe(false);
  });

  it("skips a tap that ends a text selection", () => {
    expect(shouldFocusComposerOnThreadTap({ isCoarse: true, target: plain, hasSelection: true })).toBe(false);
  });

  it("skips taps on interactive controls so they do their own thing", () => {
    const card = el('<div><button type="button"><span>expand</span></button></div>');
    const inner = card.querySelector("span")!;
    expect(shouldFocusComposerOnThreadTap({ isCoarse: true, target: inner, hasSelection: false })).toBe(false);
    const link = el('<a href="#">file.ts</a>');
    expect(shouldFocusComposerOnThreadTap({ isCoarse: true, target: link, hasSelection: false })).toBe(false);
  });

  it("tolerates a null / non-element target", () => {
    expect(shouldFocusComposerOnThreadTap({ isCoarse: true, target: null, hasSelection: false })).toBe(true);
  });
});
