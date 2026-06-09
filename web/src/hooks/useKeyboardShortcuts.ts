import { useEffect } from "react";
import { IS_MAC, matchShortcut } from "../lib/shortcuts";
import type { ShortcutActions } from "../lib/shortcuts";

export type { ShortcutActions };

/**
 * Global keyboard shortcuts for the dashboard. Bindings, help-overlay labels,
 * and tour hints all read from the single SHORTCUTS registry in lib/shortcuts.
 * This hook is the DOM seam: it decides whether the keydown target is an input,
 * delegates matching to the pure matchShortcut, and applies the effects.
 *
 * Single-key shortcuts fire only when no input/textarea/terminal is focused.
 * Cmd+K (Mac) / Ctrl+K (other) and Escape fire regardless of focus.
 */
export function useKeyboardShortcuts(getActions: () => ShortcutActions) {
  useEffect(() => {
    const handler = (e: KeyboardEvent) => {
      const target = e.target as HTMLElement | null;
      const isInput =
        !!target && (target.tagName === "INPUT" || target.tagName === "TEXTAREA" || target.isContentEditable);

      const matched = matchShortcut(e, { mac: IS_MAC, isInput });
      if (!matched) return;

      if (matched.preventDefault) e.preventDefault();
      if (matched.stopPropagation) e.stopPropagation();
      getActions()[matched.shortcut.action]();
    };

    // Capture phase so we observe the keydown before xterm.js's helper
    // textarea sees it. xterm.js calls stopPropagation on a handful of
    // modifier combos (Cmd/Ctrl + letter), which otherwise blocks global
    // shortcuts like Cmd+K, Cmd+`, and Ctrl+Alt+B whenever the terminal
    // is focused.
    document.addEventListener("keydown", handler, true);
    return () => document.removeEventListener("keydown", handler, true);
  }, [getActions]);
}
