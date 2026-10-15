// @vitest-environment jsdom

import { afterEach, expect, it, vi } from "vitest";

import { clientFormFactor } from "../formFactor";

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
  (window.navigator as unknown as { standalone?: boolean }).standalone = opts.iosStandalone ?? false;
}

afterEach(() => {
  vi.restoreAllMocks();
  delete (window.navigator as unknown as { standalone?: boolean }).standalone;
});

it("clientFormFactor is mobile only for a narrow coarse pointer, with a pwa suffix when standalone", () => {
  const cases: [Parameters<typeof stubClient>[0], string][] = [
    [{ wide: true, coarse: false }, "desktop"],
    [{ wide: false, coarse: true }, "mobile"],
    [{ wide: true, coarse: false, standalone: true }, "desktop_pwa"],
    [{ wide: false, coarse: true, standalone: true }, "mobile_pwa"],
    [{ wide: false, coarse: true, iosStandalone: true }, "mobile_pwa"],
    [{ wide: true, coarse: true }, "desktop"],
    [{ wide: false, coarse: false }, "desktop"],
  ];
  for (const [client, expected] of cases) {
    stubClient(client);
    expect(clientFormFactor(), JSON.stringify(client)).toBe(expected);
  }
});
