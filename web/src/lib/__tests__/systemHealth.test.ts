import { describe, expect, it } from "vitest";
import { formatBytes, formatPercent, parseSystemHealthEnabled } from "../systemHealth";

describe("parseSystemHealthEnabled", () => {
  it("only an explicit true enables the strip", () => {
    const cases: Array<[unknown, boolean]> = [
      [{ session: { show_diagnostics_pane: true } }, true],
      [{ session: { show_diagnostics_pane: false } }, false],
      // An older daemon omits the field, or sends something unusable.
      [{ session: {} }, false],
      [{ session: { show_diagnostics_pane: "yes" } }, false],
      [{ session: null }, false],
      [{}, false],
      [null, false],
      [undefined, false],
    ];
    for (const [settings, expected] of cases) {
      expect(parseSystemHealthEnabled(settings as Record<string, unknown> | null)).toBe(expected);
    }
  });
});

describe("formatBytes", () => {
  // The same table `format_bytes_table` pins for the TUI formatter in
  // src/tui/components/diagnostics.rs, so the two languages cannot drift
  // apart silently.
  it("matches the TUI strip's units", () => {
    const gib = 1024 ** 3;
    const cases: Array<[number, string]> = [
      [0, "0B"],
      [512, "512B"],
      [1024, "1K"],
      [536_870_912, "512M"],
      [32 * gib, "32G"],
      [22 * gib + Math.trunc((7 * gib) / 10), "22.7G"],
      [9 * gib + Math.trunc((9 * gib) / 10), "9.9G"],
    ];
    for (const [bytes, expected] of cases) {
      expect(formatBytes(bytes)).toBe(expected);
    }
  });
});

describe("formatPercent", () => {
  it("reads an absent fraction as unknown, not zero", () => {
    expect(formatPercent(0)).toBe("0%");
    expect(formatPercent(0.425)).toBe("43%");
    expect(formatPercent(null)).toBe("?");
    expect(formatPercent(undefined)).toBe("?");
  });
});
