// @vitest-environment jsdom

import { afterEach, beforeEach, describe, expect, it } from "vitest";
import { cleanup, fireEvent, render } from "@testing-library/react";
import { StructuredViewDisplaySettings } from "../StructuredViewDisplaySettings";
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
  it("keeps conversations alive off by default and clamps the limit once enabled", () => {
    const { getByTestId, queryByLabelText, getByLabelText } = render(<StructuredViewDisplaySettings />);
    expect(queryByLabelText("Conversation keep-alive limit")).toBeNull();

    fireEvent.click(getByTestId("persistent-structured-views-toggle"));
    expect(readStored().persistentStructuredViews).toBe(true);

    const limit = getByLabelText("Conversation keep-alive limit");
    fireEvent.change(limit, { target: { value: "4" } });
    expect(readStored().maxPersistentStructuredViews).toBe(4);
    fireEvent.change(limit, { target: { value: "99" } });
    expect(readStored().maxPersistentStructuredViews).toBe(5);
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

  it("drafts the history count, commits on blur or Enter, and persists the normalized count", () => {
    window.localStorage.setItem(KEY, JSON.stringify({ desktopFontSize: 18, sidebarCompact: true }));
    const first = render(<StructuredViewDisplaySettings />);
    const input = first.getByRole("spinbutton", { name: "Initial visible history" });
    expect((input as HTMLInputElement).value).toBe("150");
    fireEvent.focus(input);
    fireEvent.change(input, { target: { value: "" } });
    expect(readStored().structuredHistoryWindow).toBeUndefined();
    fireEvent.change(input, { target: { value: "300.6" } });
    expect(readStored().structuredHistoryWindow).toBeUndefined();
    fireEvent.blur(input);
    expect(readStored().structuredHistoryWindow).toBe(301);
    expect((input as HTMLInputElement).value).toBe("301");

    fireEvent.focus(input);
    fireEvent.change(input, { target: { value: "2000" } });
    fireEvent.keyDown(input, { key: "Enter" });
    expect(readStored().structuredHistoryWindow).toBe(1000);
    expect((input as HTMLInputElement).value).toBe("1000");
    expect(readStored().desktopFontSize).toBe(18);
    expect(readStored().sidebarCompact).toBe(true);
    first.unmount();
    expect(getWebSettingsSnapshot().structuredHistoryWindow).toBe(1000);
    const second = render(<StructuredViewDisplaySettings />);
    expect((second.getByRole("spinbutton", { name: "Initial visible history" }) as HTMLInputElement).value).toBe(
      "1000",
    );
  });

  it("clamps a blank history draft on commit without preventing replacement typing", () => {
    const { getByRole } = render(<StructuredViewDisplaySettings />);
    const input = getByRole("spinbutton", { name: "Initial visible history" });
    fireEvent.focus(input);
    fireEvent.change(input, { target: { value: "" } });
    fireEvent.blur(input);
    expect(readStored().structuredHistoryWindow).toBe(50);
    expect((input as HTMLInputElement).value).toBe("50");
  });

  it("normalizes old and malformed history preferences when read from storage", () => {
    for (const [stored, expected] of [
      [undefined, 150],
      [null, 150],
      ["300", 150],
      [false, 150],
      [0, 50],
      [5000, 1000],
      [300.6, 301],
    ] as Array<[unknown, number]>) {
      window.localStorage.setItem(KEY, JSON.stringify({ structuredHistoryWindow: stored }));
      expect(getWebSettingsSnapshot().structuredHistoryWindow).toBe(expected);
      const { getByRole } = render(<StructuredViewDisplaySettings />);
      expect((getByRole("spinbutton", { name: "Initial visible history" }) as HTMLInputElement).value).toBe(
        String(expected),
      );
      cleanup();
    }
  });
});
