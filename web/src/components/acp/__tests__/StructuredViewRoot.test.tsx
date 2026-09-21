// @vitest-environment jsdom
import { afterEach, describe, expect, it, vi } from "vitest";
import { cleanup, render, screen } from "@testing-library/react";

const mockKeyboard = vi.hoisted(() => ({
  current: { isMobile: false, keyboardOpen: false, keyboardHeight: 0 },
}));
vi.mock("../../../hooks/useMobileKeyboard", () => ({
  useMobileKeyboard: () => mockKeyboard.current,
}));

import { StructuredViewRoot, structuredViewRootStyle } from "../StructuredView";

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

describe("structuredViewRootStyle", () => {
  it.each([
    [280, { paddingBottom: 280 }],
    [0, undefined],
    [-12, undefined],
  ])("keyboardHeight %s -> %o", (height, expected) => {
    expect(structuredViewRootStyle(height)).toEqual(expected);
  });
});

describe("StructuredViewRoot", () => {
  it.each([
    [280, "280px"],
    [0, ""],
  ])("reserves keyboard height %s as bottom padding", (height, padding) => {
    expect(renderRoot(height).style.paddingBottom).toBe(padding);
  });

  // rem, not px, so the transcript still follows the browser's root font size.
  it("publishes both conversation font sizes as rem without dropping the keyboard reservation", () => {
    window.localStorage.setItem(
      "aoe-web-settings",
      JSON.stringify({ structuredMobileFontSize: 11, structuredDesktopFontSize: 18 }),
    );
    const root = renderRoot(300);
    expect(root.style.getPropertyValue("--acp-conversation-font-size-mobile")).toBe("0.6875rem");
    expect(root.style.getPropertyValue("--acp-conversation-font-size-desktop")).toBe("1.125rem");
    // The CSS rule choosing between them keys off this class.
    expect(root.classList.contains("acp-conversation-scope")).toBe(true);
    expect(root.style.paddingBottom).toBe("300px");
  });

  it("publishes the 14px default as 0.875rem", () => {
    const root = renderRoot(0);
    expect(root.style.getPropertyValue("--acp-conversation-font-size-mobile")).toBe("0.875rem");
    expect(root.style.getPropertyValue("--acp-conversation-font-size-desktop")).toBe("0.875rem");
  });
});
