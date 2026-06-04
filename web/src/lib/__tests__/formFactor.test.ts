// @vitest-environment jsdom

import { afterEach, describe, expect, it, vi } from "vitest";

import { clientFormFactor } from "../formFactor";

/** Drive the three media-query primitives `clientFormFactor` reads by query
 *  string, plus the iOS `navigator.standalone` flag. Every query the helper
 *  does not set is reported as not-matching. */
function stubClient(opts: {
  standalone?: boolean; // display-mode: standalone
  iosStandalone?: boolean; // navigator.standalone
  coarse?: boolean; // pointer: coarse
  wide?: boolean; // min-width: 768px
}) {
  window.matchMedia = vi.fn((query: string) => {
    const matches =
      (query === "(display-mode: standalone)" && !!opts.standalone) ||
      (query === "(pointer: coarse)" && !!opts.coarse) ||
      (query === "(min-width: 768px)" && !!opts.wide);
    return { matches, media: query } as MediaQueryList;
  }) as unknown as typeof window.matchMedia;
  (window.navigator as unknown as { standalone?: boolean }).standalone =
    opts.iosStandalone ?? false;
}

afterEach(() => {
  vi.restoreAllMocks();
  delete (window.navigator as unknown as { standalone?: boolean }).standalone;
});

describe("clientFormFactor", () => {
  it("classifies a wide fine-pointer client as desktop", () => {
    stubClient({ wide: true, coarse: false });
    expect(clientFormFactor()).toBe("desktop");
  });

  it("classifies a narrow coarse-pointer client as mobile", () => {
    stubClient({ wide: false, coarse: true });
    expect(clientFormFactor()).toBe("mobile");
  });

  it("adds the pwa suffix when running standalone", () => {
    stubClient({ wide: true, coarse: false, standalone: true });
    expect(clientFormFactor()).toBe("desktop_pwa");
  });

  it("treats an installed mobile PWA as mobile_pwa", () => {
    stubClient({ wide: false, coarse: true, standalone: true });
    expect(clientFormFactor()).toBe("mobile_pwa");
  });

  it("honors iOS navigator.standalone for the pwa suffix", () => {
    stubClient({ wide: false, coarse: true, iosStandalone: true });
    expect(clientFormFactor()).toBe("mobile_pwa");
  });

  it("keeps a wide coarse-pointer touch laptop on desktop", () => {
    // Coarse pointer alone must not flip to mobile: a touch laptop stays
    // desktop because the viewport is wide.
    stubClient({ wide: true, coarse: true });
    expect(clientFormFactor()).toBe("desktop");
  });

  it("keeps a narrow fine-pointer desktop window on desktop", () => {
    // A narrow viewport alone must not flip to mobile: a small desktop window
    // has a fine pointer.
    stubClient({ wide: false, coarse: false });
    expect(clientFormFactor()).toBe("desktop");
  });
});
