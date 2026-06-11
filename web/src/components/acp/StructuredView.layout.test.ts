// Layout-decision test for the structured-view root keyboard reservation
// added in #2011. The pure helper lets us check the inline style across the
// iOS-regular-Safari case (keyboardHeight > 0) and the layout-shrinking
// platforms (keyboardHeight == 0) without mounting the assistant-ui runtime.

import { describe, expect, it } from "vitest";

import { structuredViewRootStyle } from "./StructuredView";

describe("structuredViewRootStyle (#2011)", () => {
  it("reserves the keyboard height as bottom padding on iOS regular Safari", () => {
    expect(structuredViewRootStyle(280)).toEqual({ paddingBottom: 280 });
  });

  it("returns no style when the layout viewport already shrinks (keyboardHeight 0)", () => {
    expect(structuredViewRootStyle(0)).toBeUndefined();
  });

  it("ignores a negative measurement rather than padding upward", () => {
    expect(structuredViewRootStyle(-12)).toBeUndefined();
  });
});
