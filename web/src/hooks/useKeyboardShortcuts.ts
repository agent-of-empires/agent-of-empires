import { useEffect } from "react";
import { IS_MAC, matchShortcut } from "../lib/shortcuts";
import type { ShortcutActions } from "../lib/shortcuts";

export type { ShortcutActions };

export function useKeyboardShortcuts(getActions: () => ShortcutActions) {
  useEffect(() => {
    const handler = (e: KeyboardEvent) => {
      const target = e.target instanceof Element ? e.target : null;
      const isInput =
        target instanceof HTMLElement &&
        (target.tagName === "INPUT" || target.tagName === "TEXTAREA" || target.isContentEditable);

      // A field that marks itself closes its own open list on Escape; the surface around it stays.
      if (e.key === "Escape" && target?.closest('[data-owns-escape][aria-expanded="true"]')) return;

      const isSessionInput = !!target?.closest(
        '[data-term="agent"], [data-term="paired"], [data-session-composer], [data-keyboard-proxy][data-session-input]',
      );
      const matched = matchShortcut(e, { mac: IS_MAC, isInput, isSessionInput });
      if (!matched) return;

      if (matched.preventDefault) e.preventDefault();
      if (matched.stopPropagation) e.stopPropagation();
      getActions()[matched.shortcut.action]();
    };

    // Capture phase: xterm.js stops propagation of Cmd/Ctrl combos.
    document.addEventListener("keydown", handler, true);
    // Lets browser tests wait for the listener instead of racing the effect flush.
    document.documentElement.dataset.shortcutsReady = "true";
    return () => document.removeEventListener("keydown", handler, true);
  }, [getActions]);
}
