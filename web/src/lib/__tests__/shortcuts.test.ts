// Drift guard + behavioral lock for the single shortcut registry (issue #1648).
//
// This is the test that makes "one source of truth" real: it pins the exact
// rendered label strings (so the help overlay and tour cannot silently change
// formatting), pins the match behavior of every binding (so a refactor cannot
// rebind a key), proves the SHORTCUTS array order is cosmetic (exactly one
// shortcut matches any given event), and couples the tour to the registry
// (every tour hint id resolves to a registered shortcut).
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

function ev(partial: Partial<ShortcutKeyEvent>): ShortcutKeyEvent {
  return {
    key: "",
    code: "",
    metaKey: false,
    ctrlKey: false,
    altKey: false,
    shiftKey: false,
    ...partial,
  };
}

describe("SHORTCUTS registry", () => {
  it("has unique ids", () => {
    const ids = SHORTCUTS.map((s) => s.id);
    expect(new Set(ids).size).toBe(ids.length);
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
    rightPanel: "⌘⌥B",
    terminalFocus: "⌘`",
    new: "n",
    newScratch: "⌘⇧N",
    diff: "D",
    settings: "s",
    escape: "Esc",
    help: "?",
  };
  const helpOther: Record<string, string> = {
    palette: "CtrlK",
    sidebar: "CtrlB",
    rightPanel: "CtrlAltB",
    terminalFocus: "Ctrl`",
    new: "n",
    newScratch: "CtrlShiftN",
    diff: "D",
    settings: "s",
    escape: "Esc",
    help: "?",
  };
  const tour: Record<string, string> = {
    palette: "⌘K / Ctrl+K",
    sidebar: "⌘B / Ctrl+B",
    rightPanel: "⌘⌥B / Ctrl+Alt+B",
    terminalFocus: "⌘` / Ctrl+`",
    new: "n",
    newScratch: "⌘⇧N / Ctrl+Shift+N",
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

describe("matchShortcut behavior (no binding changed by the refactor)", () => {
  const cases: Array<{
    name: string;
    event: ShortcutKeyEvent;
    mac: boolean;
    isInput?: boolean;
    expected: ShortcutDef["id"] | null;
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
    const term = matchShortcut(
      ev({ key: "`", code: "Backquote", metaKey: true }),
      { mac: true, isInput: false },
    );
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

describe("array order is cosmetic (predicates are mutually exclusive)", () => {
  // Build the event that should fire each shortcut, then assert exactly one
  // shortcut in the whole registry matches it. If a future binding overlaps an
  // existing one, this turns red regardless of array order.
  function triggeringEvent(s: ShortcutDef): {
    event: ShortcutKeyEvent;
    isInput: boolean;
  } {
    const t = s.trigger;
    const e = ev({});
    if (t.scope === "global") {
      if (t.mod) e.metaKey = true;
      if (t.shift) e.shiftKey = true;
      if (t.alt) e.altKey = true;
      if (t.code) {
        e.code = t.code;
        e.key =
          t.code === "Backquote"
            ? "`"
            : t.code.replace(/^Key/, "").toLowerCase();
      }
      if (t.key) e.key = t.key;
    } else {
      e.key = t.key ?? "";
    }
    return { event: e, isInput: false };
  }

  function allMatchingIds(
    event: ShortcutKeyEvent,
    opts: { mac: boolean; isInput: boolean },
  ): string[] {
    const mod = opts.mac ? event.metaKey : event.metaKey || event.ctrlKey;
    const hasMetaCtrlAlt = event.metaKey || event.ctrlKey || event.altKey;
    const ids: string[] = [];
    for (const s of SHORTCUTS) {
      const t = s.trigger;
      if (t.scope === "global") {
        if (t.mod !== undefined && t.mod !== mod) continue;
        if (t.shift !== undefined && t.shift !== event.shiftKey) continue;
        if (t.alt !== undefined && t.alt !== event.altKey) continue;
        if (t.code !== undefined) {
          if (event.code !== t.code) continue;
        } else if (t.key !== undefined) {
          const ok = t.keyCaseInsensitive
            ? event.key.toLowerCase() === t.key.toLowerCase()
            : event.key === t.key;
          if (!ok) continue;
        } else {
          continue;
        }
        ids.push(s.id);
      } else {
        if (opts.isInput || hasMetaCtrlAlt) continue;
        if (event.key === t.key) ids.push(s.id);
      }
    }
    return ids;
  }

  for (const s of SHORTCUTS) {
    it(`exactly one shortcut matches the event that triggers ${s.id} (mac)`, () => {
      const { event, isInput } = triggeringEvent(s);
      expect(allMatchingIds(event, { mac: true, isInput })).toEqual([s.id]);
    });
  }
});

describe("tour drift guard", () => {
  it("every tour shortcut hint id resolves to a registered shortcut", () => {
    for (const step of TOUR_STEPS) {
      for (const hint of step.shortcutHints ?? []) {
        expect(
          SHORTCUTS_BY_ID[hint.id],
          `step "${step.id}" hint "${hint.id}" is not registered`,
        ).toBeDefined();
      }
    }
  });
});
