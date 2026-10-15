// @vitest-environment jsdom
import { afterEach, describe, expect, it, vi } from "vitest";
import { cleanup, render, screen } from "@testing-library/react";

const mockKeyboard = vi.hoisted(() => ({
  current: { isMobile: false, keyboardOpen: false, keyboardHeight: 0 },
}));
vi.mock("../../../hooks/useMobileKeyboard", () => ({
  useMobileKeyboard: () => mockKeyboard.current,
}));

import { StructuredViewRoot } from "../StructuredView";

afterEach(() => {
  cleanup();
  window.localStorage.clear();
  mockKeyboard.current = { isMobile: false, keyboardOpen: false, keyboardHeight: 0 };
});

function renderRoot(keyboardHeight: number) {
  mockKeyboard.current = { isMobile: true, keyboardOpen: keyboardHeight > 0, keyboardHeight };
  render(
    <StructuredViewRoot>
      <div>child</div>
    </StructuredViewRoot>,
  );
  return screen.getByTestId("structured-view-root");
}

describe("StructuredViewRoot", () => {
  it.each([
    [280, "280px"],
    [0, ""],
    [-12, ""],
  ])("reserves keyboard height %s as bottom padding", (height, padding) => {
    expect(renderRoot(height).style.paddingBottom).toBe(padding);
  });

  // rem, not px, so the transcript still follows the browser's root font size.
  it("publishes both conversation font sizes as rem, defaulting to 14px, without dropping the keyboard reservation", () => {
    const fontSizes = (root: HTMLElement) => [
      root.style.getPropertyValue("--acp-conversation-font-size-mobile"),
      root.style.getPropertyValue("--acp-conversation-font-size-desktop"),
    ];
    expect(fontSizes(renderRoot(0))).toEqual(["0.875rem", "0.875rem"]);
    cleanup();
    window.localStorage.setItem(
      "aoe-web-settings",
      JSON.stringify({ structuredMobileFontSize: 11, structuredDesktopFontSize: 18 }),
    );
    const root = renderRoot(300);
    expect(fontSizes(root)).toEqual(["0.6875rem", "1.125rem"]);
    // The CSS rule choosing between them keys off this class.
    expect(root.classList.contains("acp-conversation-scope")).toBe(true);
    expect(root.style.paddingBottom).toBe("300px");
  });
});
