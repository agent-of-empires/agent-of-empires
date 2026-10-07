import { describe, expect, it } from "vitest";
import {
  SHORTCUTS,
  SHORTCUTS_BY_ID,
  type ShortcutDef,
  type ShortcutKeyEvent,
  formatHelpShortcut,
  matchShortcut,
  formatTourShortcut,
} from "../shortcuts";
import { TOUR_STEPS } from "../tourSteps";

// Ctrl+Q matches only while an embedded terminal or structured composer owns focus; bare keys remain textless.
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

  it("SHORTCUTS_BY_ID resolves every entry", () => {
    for (const s of SHORTCUTS) {
      expect(SHORTCUTS_BY_ID[s.id]).toBe(s);
    }
  });
});

describe("label formatting (locked byte-for-byte against pre-refactor output)", () => {
  const helpMac: Record<string, string> = {
    palette: "⌘K",
    sidebar: "⌘B",
    sidebarFocus: "⌃Q",
    rightPanel: "⌘⌥B",
    terminalFocus: "⌘`",
    new: "n",
    newScratch: "⌘⇧N",
    jumpAttention: "a",
    diff: "D",
    settings: "s",
    escape: "Esc",
    help: "?",
  };
  const helpOther: Record<string, string> = {
    palette: "CtrlK",
    sidebar: "CtrlB",
    sidebarFocus: "CtrlQ",
    rightPanel: "CtrlAltB",
    terminalFocus: "Ctrl`",
    new: "n",
    newScratch: "CtrlShiftN",
    jumpAttention: "a",
    diff: "D",
    settings: "s",
    escape: "Esc",
    help: "?",
  };
  const tour: Record<string, string> = {
    palette: "⌘K / Ctrl+K",
    sidebar: "⌘B / Ctrl+B",
    sidebarFocus: "⌃Q / Ctrl+Q",
    rightPanel: "⌘⌥B / Ctrl+Alt+B",
    terminalFocus: "⌘` / Ctrl+`",
    new: "n",
    newScratch: "⌘⇧N / Ctrl+Shift+N",
    jumpAttention: "a",
    diff: "D",
    settings: "s",
    escape: "Esc",
    help: "?",
  };

  for (const s of SHORTCUTS) {
    it(`${s.id} renders the expected help (mac/other) and tour strings`, () => {
      expect(formatHelpShortcut(s.chord, true)).toBe(helpMac[s.id]);
      expect(formatHelpShortcut(s.chord, false)).toBe(helpOther[s.id]);
      expect(formatTourShortcut(s.chord)).toBe(tour[s.id]);
    });
  }
});

describe("matchShortcut behavior", () => {
  const cases: Array<{
    name: string;
    event: ShortcutKeyEvent;
    mac: boolean;
    isInput?: boolean;
    expected: ShortcutDef["id"] | null;
    isSessionInput?: boolean;
  }> = [
    {
      name: "mac Meta+K -> palette",
      event: ev({ key: "k", metaKey: true }),
      mac: true,
      expected: "palette",
    },
    {
      name: "mac Ctrl+K -> no match",
      event: ev({ key: "k", ctrlKey: true }),
      mac: true,
      expected: null,
    },
    {
      name: "other Ctrl+K -> palette",
      event: ev({ key: "k", ctrlKey: true }),
      mac: false,
      expected: "palette",
    },
    {
      name: "other Meta+K -> palette",
      event: ev({ key: "k", metaKey: true }),
      mac: false,
      expected: "palette",
    },
    {
      name: "palette fires inside an input",
      event: ev({ key: "k", metaKey: true }),
      mac: true,
      isInput: true,
      expected: "palette",
    },
    {
      name: "Meta+Backquote -> terminalFocus",
      event: ev({ key: "`", code: "Backquote", metaKey: true }),
      mac: true,
      expected: "terminalFocus",
    },
    {
      name: "Meta+Alt+B (KeyB) -> rightPanel",
      event: ev({ key: "b", code: "KeyB", metaKey: true, altKey: true }),
      mac: true,
      expected: "rightPanel",
    },
    {
      name: "Meta+B (KeyB) -> sidebar",
      event: ev({ key: "b", code: "KeyB", metaKey: true }),
      mac: true,
      expected: "sidebar",
    },
    {
      name: "Ctrl+Q returns focus to the sidebar from the session input on Mac",
      event: ev({ key: "q", code: "KeyQ", ctrlKey: true }),
      mac: true,
      isInput: true,
      isSessionInput: true,
      expected: "sidebarFocus",
    },
    {
      name: "Ctrl+Q returns focus to the sidebar from the session input on other platforms",
      event: ev({ key: "q", code: "KeyQ", ctrlKey: true }),
      mac: false,
      isInput: true,
      isSessionInput: true,
      expected: "sidebarFocus",
    },
    {
      name: "Ctrl+Q outside a session input is left to the browser",
      event: ev({ key: "q", code: "KeyQ", ctrlKey: true }),
      mac: false,
      isInput: true,
      expected: null,
    },
    {
      name: "Cmd+Q remains the browser shortcut",
      event: ev({ key: "q", code: "KeyQ", metaKey: true }),
      mac: true,
      isSessionInput: true,
      expected: null,
    },
    {
      name: "Mac Option+B (key '∫', code KeyB) still -> rightPanel",
      event: ev({ key: "∫", code: "KeyB", metaKey: true, altKey: true }),
      mac: true,
      expected: "rightPanel",
    },
    {
      name: "Meta+Shift+N -> newScratch",
      event: ev({ key: "N", code: "KeyN", metaKey: true, shiftKey: true }),
      mac: true,
      expected: "newScratch",
    },
    {
      name: "newScratch fires inside an input",
      event: ev({ key: "N", code: "KeyN", metaKey: true, shiftKey: true }),
      mac: true,
      isInput: true,
      expected: "newScratch",
    },
    {
      name: "Escape -> escape (no modifiers)",
      event: ev({ key: "Escape" }),
      mac: true,
      expected: "escape",
    },
    {
      name: "Escape fires inside an input",
      event: ev({ key: "Escape" }),
      mac: true,
      isInput: true,
      expected: "escape",
    },
    {
      name: "Escape fires even with a modifier",
      event: ev({ key: "Escape", metaKey: true }),
      mac: true,
      expected: "escape",
    },
    { name: "n -> new", event: ev({ key: "n" }), mac: true, expected: "new" },
    {
      name: "a -> jumpAttention",
      event: ev({ key: "a" }),
      mac: true,
      expected: "jumpAttention",
    },
    {
      name: "N (no mod) -> no match (case sensitive)",
      event: ev({ key: "N" }),
      mac: true,
      expected: null,
    },
    { name: "D -> diff", event: ev({ key: "D" }), mac: true, expected: "diff" },
    {
      name: "d -> no match (case sensitive)",
      event: ev({ key: "d" }),
      mac: true,
      expected: null,
    },
    {
      name: "s -> settings",
      event: ev({ key: "s" }),
      mac: true,
      expected: "settings",
    },
    {
      name: "S -> no match (case sensitive)",
      event: ev({ key: "S" }),
      mac: true,
      expected: null,
    },
    { name: "? -> help", event: ev({ key: "?" }), mac: true, expected: "help" },
    {
      name: "single-key blocked inside an input",
      event: ev({ key: "n" }),
      mac: true,
      isInput: true,
      expected: null,
    },
    {
      name: "single-key blocked when Ctrl held",
      event: ev({ key: "n", ctrlKey: true }),
      mac: true,
      expected: null,
    },
    {
      name: "single-key blocked when Alt held",
      event: ev({ key: "n", altKey: true }),
      mac: true,
      expected: null,
    },
  ];

  for (const c of cases) {
    it(c.name, () => {
      const matched = matchShortcut(c.event, {
        mac: c.mac,
        isInput: c.isInput ?? false,
        isSessionInput: c.isSessionInput ?? false,
      });
      expect(matched?.shortcut.id ?? null).toBe(c.expected);
    });
  }

  it("propagates the per-shortcut preventDefault / stopPropagation flags", () => {
    const palette = matchShortcut(ev({ key: "k", metaKey: true }), {
      mac: true,
      isInput: false,
    });
    expect(palette).toMatchObject({
      preventDefault: true,
      stopPropagation: true,
    });

    // terminalFocus deliberately does not stopPropagation.
    const term = matchShortcut(ev({ key: "`", code: "Backquote", metaKey: true }), { mac: true, isInput: false });
    expect(term).toMatchObject({
      preventDefault: true,
      stopPropagation: false,
    });

    // escape neither prevents nor stops.
    const esc = matchShortcut(ev({ key: "Escape" }), {
      mac: true,
      isInput: false,
    });
    expect(esc).toMatchObject({
      preventDefault: false,
      stopPropagation: false,
    });
  });
});

describe("tour drift guard", () => {
  it("every tour shortcut hint id resolves to a registered shortcut", () => {
    for (const step of TOUR_STEPS) {
      for (const hint of step.shortcutHints ?? []) {
        expect(SHORTCUTS_BY_ID[hint.id], `step "${step.id}" hint "${hint.id}"`).toBeDefined();
      }
    }
  });
});
