// @vitest-environment jsdom
//
// Contract test for the structured-view conversation font-size controls. Like
// DiffSettings / TerminalSettings these are dashboard preferences persisted
// through useWebSettings + localStorage (key `aoe-web-settings`, which
// webUiSync mirrors server-side), never PATCH /api/settings, so the contract is
// the JSON shape written to that key plus the defaulting/clamping applied when
// reading it back.

import { afterEach, beforeEach, describe, expect, it } from "vitest";
import { cleanup, fireEvent, render } from "@testing-library/react";
import { StructuredViewDisplaySettings } from "../StructuredViewDisplaySettings";
import { TerminalSettings } from "../../TerminalSettings";
import { getWebSettingsSnapshot } from "../../../hooks/useWebSettings";

const KEY = "aoe-web-settings";

function readStored(): Record<string, unknown> {
  const raw = window.localStorage.getItem(KEY);
  return raw ? (JSON.parse(raw) as Record<string, unknown>) : {};
}

beforeEach(() => {
  window.localStorage.clear();
});

afterEach(cleanup);

describe("StructuredViewDisplaySettings localStorage contract", () => {
  it("defaults both sliders to 14px", () => {
    const { getByTestId } = render(<StructuredViewDisplaySettings />);
    expect((getByTestId("structured-mobile-font-size-slider") as HTMLInputElement).value).toBe("14");
    expect((getByTestId("structured-desktop-font-size-slider") as HTMLInputElement).value).toBe("14");
  });

  // The two panels are the only font-size sliders in the dashboard and users
  // compare them side by side, so they span one shared range. Rendering both
  // catches a panel that re-hardcodes its own bounds.
  it("spans the same slider range as the terminal font-size controls", () => {
    const conversation = render(<StructuredViewDisplaySettings />);
    const terminal = render(<TerminalSettings />);
    const bounds = (root: ReturnType<typeof render>, testId: string) => {
      const el = root.getByTestId(testId) as HTMLInputElement;
      return [el.min, el.max];
    };
    const expected = bounds(terminal, "terminal-mobile-font-size-slider");
    expect(expected).toEqual(["6", "28"]);
    for (const testId of [
      "terminal-desktop-font-size-slider",
      "structured-mobile-font-size-slider",
      "structured-desktop-font-size-slider",
    ]) {
      const root = testId.startsWith("terminal-") ? terminal : conversation;
      expect(bounds(root, testId), testId).toEqual(expected);
      expect([...root.getByTestId(testId.replace("-slider", "-select")).children].length, testId).toBe(23);
    }
  });

  it("writes each axis independently and leaves the terminal sizes alone", () => {
    const { getByTestId } = render(<StructuredViewDisplaySettings />);

    fireEvent.change(getByTestId("structured-mobile-font-size-slider"), { target: { value: "11" } });
    expect(readStored().structuredMobileFontSize).toBe(11);
    expect(readStored().structuredDesktopFontSize).toBe(14);

    fireEvent.change(getByTestId("structured-desktop-font-size-select"), { target: { value: "18" } });
    expect(readStored().structuredMobileFontSize).toBe(11);
    expect(readStored().structuredDesktopFontSize).toBe(18);

    // The terminal font sizes are a separate preference and must not move.
    expect(readStored().mobileFontSize).toBe(8);
    expect(readStored().desktopFontSize).toBe(14);
  });

  it("keeps the slider and the px select synchronized on both axes", () => {
    const { getByTestId } = render(<StructuredViewDisplaySettings />);

    fireEvent.change(getByTestId("structured-mobile-font-size-select"), { target: { value: "20" } });
    expect((getByTestId("structured-mobile-font-size-slider") as HTMLInputElement).value).toBe("20");

    fireEvent.change(getByTestId("structured-desktop-font-size-slider"), { target: { value: "10" } });
    expect((getByTestId("structured-desktop-font-size-select") as HTMLInputElement).value).toBe("10");
  });

  it("survives a reread and reflects the stored values on remount", () => {
    const first = render(<StructuredViewDisplaySettings />);
    fireEvent.change(first.getByTestId("structured-mobile-font-size-slider"), { target: { value: "12" } });
    cleanup();

    expect(getWebSettingsSnapshot().structuredMobileFontSize).toBe(12);
    const { getByTestId } = render(<StructuredViewDisplaySettings />);
    expect((getByTestId("structured-mobile-font-size-slider") as HTMLInputElement).value).toBe("12");
  });

  it("backfills settings saved before these fields existed, and clamps junk", () => {
    const cases: Array<[unknown, unknown, number, number]> = [
      // [stored mobile, stored desktop, expected mobile, expected desktop]
      [undefined, undefined, 14, 14], // pre-existing settings blob
      [0, -5, 6, 6], // a zero/negative size must never reach CSS
      ["9999", 1e9, 28, 28], // absurd values clamp to the max
      ["16", "13", 16, 13], // stringy but sane values coerce
      [Number.NaN, "not a number", 14, 14], // NaN falls back to the default
      [12.4, 12.6, 12, 13], // fractional values round
    ];
    for (const [mobile, desktop, expectedMobile, expectedDesktop] of cases) {
      window.localStorage.setItem(
        KEY,
        JSON.stringify({ mobileFontSize: 8, structuredMobileFontSize: mobile, structuredDesktopFontSize: desktop }),
      );
      const { getByTestId } = render(<StructuredViewDisplaySettings />);
      const label = `${String(mobile)}/${String(desktop)}`;
      expect((getByTestId("structured-mobile-font-size-slider") as HTMLInputElement).value, label).toBe(
        String(expectedMobile),
      );
      expect((getByTestId("structured-desktop-font-size-slider") as HTMLInputElement).value, label).toBe(
        String(expectedDesktop),
      );
      cleanup();
    }
  });
});
