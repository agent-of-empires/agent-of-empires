// @vitest-environment jsdom

import { beforeEach, describe, expect, it } from "vitest";
import { getWebSettingsSnapshot } from "../../hooks/useWebSettings";
import {
  normalizeTerminalFontFamily,
  terminalFontChoices,
  terminalFontStack,
  terminalLineRatio,
} from "../terminalFonts";

describe("normalizeTerminalFontFamily", () => {
  it.each([
    ["", ""],
    ["JetBrains Mono", "JetBrains Mono"],
    ["  jetbrains mono ", "JetBrains Mono"],
    ['"Fira Code"', "Fira Code"],
    ["'menlo'", "Menlo"],
    ["meslolgs nf", "MesloLGS NF"],
    // The default by name, an unlisted font, and a font stack all fall back to the default.
    ["Geist Mono", ""],
    ["Comic Mono", ""],
    ['"Fira Code", monospace', ""],
    [undefined, ""],
    [42, ""],
  ] as [unknown, string][])("maps %j to %j", (stored, want) => {
    expect(normalizeTerminalFontFamily(stored)).toBe(want);
  });

  it("normalizes the stored setting at read time", () => {
    window.localStorage.setItem("aoe-web-settings", JSON.stringify({ terminalFontFamily: " 'fira code' " }));
    expect(getWebSettingsSnapshot().terminalFontFamily).toBe("Fira Code");
    window.localStorage.setItem("aoe-web-settings", JSON.stringify({ terminalFontFamily: "Typed Font" }));
    expect(getWebSettingsSnapshot().terminalFontFamily).toBe("");
  });
});

describe("terminalFontChoices", () => {
  beforeEach(() => window.localStorage.clear());

  it("lists detected system fonts in list order and ignores unlisted detections", () => {
    expect(terminalFontChoices("", ["Hack", "Menlo", "Unlisted"])).toEqual({
      installed: ["Menlo", "Hack"],
      missing: null,
    });
  });

  it.each([
    ["a detected system font", "Menlo", ["Menlo"], null],
    ["a system font this device lacks", "Fira Code", ["Menlo"], "Fira Code"],
    ["a bundled font, never probed", "JetBrains Mono", [], null],
    ["the default", "", [], null],
  ] as [string, string, string[], string | null][])("reports %s", (_name, saved, detected, missing) => {
    expect(terminalFontChoices(saved, detected).missing).toBe(missing);
  });
});

describe("terminal font rendering", () => {
  it("falls back to the bundled stack and sizes rows from bundled metrics", () => {
    expect(terminalFontStack("")).toBeUndefined();
    expect(terminalFontStack("Fira Code")).toBe('"Fira Code", var(--font-mono)');
    expect(terminalLineRatio("JetBrains Mono")).toBe(1.32);
    expect(terminalLineRatio("")).toBe(1.2);
    expect(terminalLineRatio("Menlo")).toBe(1.2);
  });
});
