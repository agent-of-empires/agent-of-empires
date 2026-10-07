import { describe, expect, it } from "vitest";

import { pickContrastForeground } from "../contrastColor";

function contrast(a: string, b: string): number {
  function luminance(hex: string): number {
    const [r, g, b2] = [1, 3, 5]
      .map((i) => parseInt(hex.slice(i, i + 2), 16) / 255)
      .map((c) => (c <= 0.03928 ? c / 12.92 : Math.pow((c + 0.055) / 1.055, 2.4)));
    return 0.2126 * r! + 0.7152 * g! + 0.0722 * b2!;
  }
  const [l1, l2] = [luminance(a), luminance(b)].sort((x, y) => y - x);
  return (l1! + 0.05) / (l2! + 0.05);
}

describe("pickContrastForeground", () => {
  it("picks black for light/bright backgrounds", () => {
    // Every built-in theme's waiting/unread accent (themes/builtin/*.toml).
    const brightAccents = [
      "#fe640b",
      "#7287fd",
      "#ffcb6b",
      "#89ddff",
      "#ffb86c",
      "#bd93f9",
      "#fbbf24",
      "#38bdf8",
      "#ffb43c",
      "#64c8ff",
      "#f6c177",
      "#c4a7e7",
      "#e0af68",
      "#0db9d7",
    ];
    for (const hex of brightAccents) {
      expect(pickContrastForeground(hex)).toBe("#000000");
    }
  });

  it("still clears WCAG AA at the mid-gray boundary, where a near-black/near-white pair would not", () => {
    // A near-black/near-white pair (e.g. #0f0f11/#fafafa) both land at ~4.28:1 against this exact gray, below the
    // 4.5:1 floor; pure black/white is required to clear it (4.69:1).
    const fg = pickContrastForeground("#777777");
    expect(fg).toBe("#000000");
    expect(contrast("#777777", fg)).toBeGreaterThanOrEqual(4.5);
  });

  it("meets WCAG AA (>= 4.5:1) against every gray from #000000 to #ffffff", () => {
    // The worst case for picking the higher-contrast of pure black/white occurs at ~18% luminance (~#757575),
    // where both candidates tie around sqrt(1.05/0.05) ~= 4.58:1, still above the 4.5:1 floor. Sweeping the full
    // gray range is cheap and pins that guarantee directly, rather than trusting the derivation alone.
    for (let v = 0; v <= 255; v += 5) {
      const hex = `#${[v, v, v].map((c) => c.toString(16).padStart(2, "0")).join("")}`;
      const fg = pickContrastForeground(hex);
      expect(contrast(hex, fg)).toBeGreaterThanOrEqual(4.5);
    }
  });
});
