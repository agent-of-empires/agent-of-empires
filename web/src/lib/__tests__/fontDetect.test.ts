// @vitest-environment jsdom
//
// The width-probe detector: a candidate is "installed" iff its measured width
// differs from the baseline generic. jsdom has no real font metrics, so we
// stub the canvas 2d context to model an installed set.

import { afterEach, describe, expect, it, vi } from "vitest";
import { detectInstalledFonts } from "../fontDetect";

const BASE: Record<string, number> = { monospace: 100, serif: 110, "sans-serif": 120 };

function stubCanvas(installed: Set<string>) {
  const ctx = {
    font: "",
    measureText() {
      // font looks like `48px monospace` or `48px "Name", monospace`.
      const m = this.font.match(/^\d+px (?:"([^"]+)", )?(\S+)$/);
      const name = m?.[1];
      const baseline = m?.[2] ?? "monospace";
      const base = BASE[baseline] ?? 100;
      return { width: name && installed.has(name) ? base + 7 : base };
    },
  };
  vi.spyOn(HTMLCanvasElement.prototype, "getContext").mockReturnValue(ctx as unknown as CanvasRenderingContext2D);
}

afterEach(() => vi.restoreAllMocks());

describe("detectInstalledFonts", () => {
  it("returns only candidates whose metrics differ from the baseline", () => {
    stubCanvas(new Set(["Menlo", "Hack"]));
    expect(detectInstalledFonts(["Menlo", "Hack", "Nonexistent Font"])).toEqual(["Menlo", "Hack"]);
  });

  it("returns [] when no 2d context is available", () => {
    vi.spyOn(HTMLCanvasElement.prototype, "getContext").mockReturnValue(null);
    expect(detectInstalledFonts(["Menlo"])).toEqual([]);
  });
});
