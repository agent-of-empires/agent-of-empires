// @vitest-environment jsdom
//
// Wiring test for the structured-view root keyboard reservation (#2011).
// StructuredViewRoot is the tiny exported shell that calls useMobileKeyboard
// and applies structuredViewRootStyle, extracted so this hook-to-style path is
// covered without mounting the assistant-ui runtime (the #1282 pattern). The
// pure helper is unit-tested separately in StructuredView.layout.test.ts; this
// asserts the hook value actually reaches the rendered root's inline style.

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
  mockKeyboard.current = { isMobile: false, keyboardOpen: false, keyboardHeight: 0 };
});

describe("StructuredViewRoot (#2011)", () => {
  it("reserves the measured keyboard height as bottom padding (iOS regular Safari)", () => {
    mockKeyboard.current = { isMobile: true, keyboardOpen: true, keyboardHeight: 280 };
    render(
      <StructuredViewRoot>
        <div>child</div>
      </StructuredViewRoot>,
    );
    expect(screen.getByTestId("structured-view-root").style.paddingBottom).toBe("280px");
  });

  it("reserves nothing when the layout viewport already shrinks (keyboardHeight 0)", () => {
    render(
      <StructuredViewRoot>
        <div>child</div>
      </StructuredViewRoot>,
    );
    expect(screen.getByTestId("structured-view-root").style.paddingBottom).toBe("");
  });
});
