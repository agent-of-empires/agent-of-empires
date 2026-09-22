import { useEffect } from "react";
import { IS_MAC, matchShortcut } from "../lib/shortcuts";
import type { ShortcutActions } from "../lib/shortcuts";

export type { ShortcutActions };

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

    // Capture phase: xterm.js stops propagation of Cmd/Ctrl combos.
    document.addEventListener("keydown", handler, true);
    return () => document.removeEventListener("keydown", handler, true);
  }, [getActions]);
}
