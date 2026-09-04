// Layout-decision tests for the structured view composer's outer wrapper added
// in #1143. The pure helper lets us check the className + inline style
// across keyboard-open / keyboard-closed without mounting the whole
// composer + assistant-ui runtime.

import { describe, expect, it } from "vitest";

import { composerWrapperLayout, IOS_ACCESSORY_BAR_PX } from "./Composer";

describe("composerWrapperLayout (#1143)", () => {
  it("uses just the base pb-3 gap and no inline style when the keyboard is closed", () => {
    const layout = composerWrapperLayout({ keyboardOpen: false });
    // The App root no longer reserves the bottom inset, so there is nothing to
    // cancel: no negative margin. The composer hugs the physical bottom with
    // only the base pb-3 gap.
    expect(layout.className).toContain("pb-3");
    expect(layout.className).not.toContain("pb-0");
    expect(layout.style).toBeUndefined();
  });

  it("drops to pb-0 and adds no inline style when the keyboard is open (no accessory bar)", () => {
    const layout = composerWrapperLayout({ keyboardOpen: true });
    expect(layout.className).toContain("pb-0");
    expect(layout.className).not.toContain("pb-3");
    expect(layout.style).toBeUndefined();
  });

  it("ignores accessoryBarPx while the keyboard is closed", () => {
    const layout = composerWrapperLayout({ keyboardOpen: false, accessoryBarPx: IOS_ACCESSORY_BAR_PX });
    // The accessory bar only exists while the keyboard is up, so the closed
    // layout is unaffected by it: pb-3 in className, no inline style.
    expect(layout.className).toContain("pb-3");
    expect(layout.style).toBeUndefined();
  });

  it("adds paddingBottom clearance for the iOS accessory bar when open", () => {
    const layout = composerWrapperLayout({ keyboardOpen: true, accessoryBarPx: IOS_ACCESSORY_BAR_PX });
    expect(layout.className).toContain("pb-0");
    expect(layout.style).toEqual({ paddingBottom: IOS_ACCESSORY_BAR_PX });
  });

  it("omits the inline style when accessoryBarPx is zero (non-iOS-PWA)", () => {
    const layout = composerWrapperLayout({ keyboardOpen: true, accessoryBarPx: 0 });
    expect(layout.style).toBeUndefined();
  });

  it("preserves shared base classes regardless of keyboard state", () => {
    for (const keyboardOpen of [true, false]) {
      const layout = composerWrapperLayout({ keyboardOpen });
      expect(layout.className).toContain("border-t");
      expect(layout.className).toContain("border-surface-800");
      expect(layout.className).toContain("bg-surface-900");
      expect(layout.className).toContain("px-4");
      expect(layout.className).toContain("pt-3");
    }
  });
});
