import { describe, expect, it } from "vitest";
import {
  SHORTCUTS,
  SHORTCUTS_BY_ID,
  type ShortcutDef,
  type ShortcutKeyEvent,
  formatHelpShortcut,
  formatTourShortcut,
  matchShortcut,
} from "../shortcuts";
import { TOUR_STEPS } from "../tourSteps";

const ev = (partial: Partial<ShortcutKeyEvent>): ShortcutKeyEvent => ({
  key: "",
  code: "",
  metaKey: false,
  ctrlKey: false,
  altKey: false,
  shiftKey: false,
  ...partial,
});

describe("SHORTCUTS registry", () => {
  it("has unique ids that SHORTCUTS_BY_ID resolves", () => {
    expect(new Set(SHORTCUTS.map((s) => s.id)).size).toBe(SHORTCUTS.length);
    for (const s of SHORTCUTS) expect(SHORTCUTS_BY_ID[s.id]).toBe(s);
  });

  it("every tour shortcut hint resolves to a registered shortcut", () => {
    for (const step of TOUR_STEPS) {
      for (const hint of step.shortcutHints ?? []) {
        expect(SHORTCUTS_BY_ID[hint.id], `step "${step.id}" hint "${hint.id}"`).toBeDefined();
      }
    }
  });
});

it.each<[ShortcutDef["id"], string, string, string]>([
  ["palette", "⌘K", "CtrlK", "⌘K / Ctrl+K"],
  ["rightPanel", "⌘⌥B", "CtrlAltB", "⌘⌥B / Ctrl+Alt+B"],
  ["terminalFocus", "⌘`", "Ctrl`", "⌘` / Ctrl+`"],
  ["new", "n", "n", "n"],
  ["newScratch", "⌘⇧N", "CtrlShiftN", "⌘⇧N / Ctrl+Shift+N"],
  ["escape", "Esc", "Esc", "Esc"],
])("%s renders as %s (mac), %s (other), and %s (tour)", (id, mac, other, tour) => {
  const { chord } = SHORTCUTS_BY_ID[id]!;
  expect([formatHelpShortcut(chord, true), formatHelpShortcut(chord, false), formatTourShortcut(chord)]).toEqual([
    mac,
    other,
    tour,
  ]);
});

describe("matchShortcut", () => {
  const metaK = ev({ key: "k", metaKey: true });
  const scratch = ev({ key: "N", code: "KeyN", metaKey: true, shiftKey: true });

  it.each<[string, ShortcutKeyEvent, boolean, boolean, ShortcutDef["id"] | null]>([
    ["mac Ctrl+K", ev({ key: "k", ctrlKey: true }), true, false, null],
    ["other Ctrl+K", ev({ key: "k", ctrlKey: true }), false, false, "palette"],
    ["other Meta+K", metaK, false, false, "palette"],
    ["Meta+K in an input", metaK, true, true, "palette"],
    [
      "Mac Option+B producing ∫",
      ev({ key: "∫", code: "KeyB", metaKey: true, altKey: true }),
      true,
      false,
      "rightPanel",
    ],
    ["Meta+Shift+N", scratch, true, false, "newScratch"],
    ["Escape in an input", ev({ key: "Escape" }), true, true, "escape"],
    ["Meta+Escape", ev({ key: "Escape", metaKey: true }), true, false, "escape"],
    ["N", ev({ key: "N" }), true, false, null],
    ["n in an input", ev({ key: "n" }), true, true, null],
    ["Ctrl+n", ev({ key: "n", ctrlKey: true }), true, false, null],
    ["Alt+n", ev({ key: "n", altKey: true }), true, false, null],
  ])("%s", (_name, event, mac, isInput, expected) => {
    expect(matchShortcut(event, { mac, isInput })?.shortcut.id ?? null).toBe(expected);
  });

  it.each(SHORTCUTS)("no earlier shortcut shadows $id", (s) => {
    const t = s.trigger;
    const event =
      t.scope === "global"
        ? ev({
            metaKey: !!t.mod,
            shiftKey: !!t.shift,
            altKey: !!t.alt,
            code: t.code ?? "",
            key: t.key ?? (t.code === "Backquote" ? "`" : (t.code ?? "").replace(/^Key/, "").toLowerCase()),
          })
        : ev({ key: t.key ?? "" });
    expect(matchShortcut(event, { mac: true, isInput: false })?.shortcut.id).toBe(s.id);
  });

  it.each<[ShortcutKeyEvent, boolean, boolean]>([
    [metaK, true, true],
    [ev({ key: "`", code: "Backquote", metaKey: true }), true, false],
    [ev({ key: "Escape" }), false, false],
  ])("propagates preventDefault/stopPropagation flags (%#)", (event, preventDefault, stopPropagation) => {
    expect(matchShortcut(event, { mac: true, isInput: false })).toMatchObject({ preventDefault, stopPropagation });
  });
});
