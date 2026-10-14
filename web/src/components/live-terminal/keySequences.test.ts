import { describe, expect, it } from "vitest";
import { altPrintableMetaKey } from "./keySequences";

describe("altPrintableMetaKey", () => {
  it.each([
    // Czech Option+u-ring types ";" and Option+Shift+7 on German types "\", so they are text, not Meta.
    ["macOS Option-layer punctuation", { key: ";", code: "Semicolon", shiftKey: false }, true, null],
    ["macOS Option+Shift ASCII", { key: "\\", code: "Digit7", shiftKey: true }, true, null],
    ["non-mac Alt+punctuation", { key: ";", code: "Semicolon", shiftKey: false }, false, ";"],
    ["non-mac Alt+Shift+digit", { key: "!", code: "Digit1", shiftKey: true }, false, "!"],
    ["non-mac Alt+letter", { key: "v", code: "KeyV", shiftKey: false }, false, "v"],
    ["macOS synthetic Alt+letter keeps Meta", { key: "v", code: "KeyV", shiftKey: false }, true, "v"],
    ["macOS Option+letter composes a symbol", { key: "√", code: "KeyV", shiftKey: false }, true, "v"],
    ["macOS Option+Shift+letter", { key: "◊", code: "KeyV", shiftKey: true }, true, "V"],
    ["macOS dead key", { key: "Dead", code: "KeyE", shiftKey: false }, true, null],
  ])("%s", (_name, event, isMac, expected) => {
    expect(altPrintableMetaKey(event, null, isMac)).toBe(expected);
  });
});
