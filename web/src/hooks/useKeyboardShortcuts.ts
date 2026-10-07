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
 * Ctrl+Q returns focus from an embedded terminal or structured composer to the session sidebar.
 */
export function useKeyboardShortcuts(getActions: () => ShortcutActions) {
  useEffect(() => {
    const handler = (e: KeyboardEvent) => {
      const target = e.target as HTMLElement | null;
      const isInput =
        !!target && (target.tagName === "INPUT" || target.tagName === "TEXTAREA" || target.isContentEditable);

      const isSessionInput = !!target?.closest('[data-term="agent"], [data-term="paired"], [data-session-composer]');
      const matched = matchShortcut(e, { mac: IS_MAC, isInput, isSessionInput });
      if (!matched) return;

      if (matched.preventDefault) e.preventDefault();
      if (matched.stopPropagation) e.stopPropagation();
      getActions()[matched.shortcut.action]();
    };

    // Capture phase: xterm.js stops propagation of Cmd/Ctrl combos.
    document.addEventListener("keydown", handler, true);
    return () => document.removeEventListener("keydown", handler, true);
  }, [getActions]);
}
