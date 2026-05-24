import { useEffect } from "react";
import type { ProjectStripShortcut } from "./useWebSettings";

const IS_MAC =
  typeof navigator !== "undefined" &&
  /Mac|iPhone|iPad|iPod/.test(navigator.platform);

interface ShortcutActions {
  onNew: () => void;
  onDiff: () => void;
  onEscape: () => void;
  onHelp: () => void;
  onSettings: () => void;
  onPalette: () => void;
  onToggleSidebar: () => void;
  onToggleRightPanel: () => void;
  onToggleTerminalFocus: () => void;
  onPreviousProject?: () => void;
  onNextProject?: () => void;
  projectStripShortcut?: ProjectStripShortcut;
}

/**
 * Global keyboard shortcuts for the dashboard.
 * Single-key shortcuts fire only when no input/textarea/terminal is focused.
 * Cmd+K (Mac) / Ctrl+K (other) and Escape fire regardless of focus.
 */
export function useKeyboardShortcuts(getActions: () => ShortcutActions) {
  useEffect(() => {
    const matchesProjectNavigationShortcut = (
      e: KeyboardEvent,
      shortcut: ProjectStripShortcut,
    ) => {
      if (shortcut === "disabled") return false;
      if (e.metaKey || (e.code !== "KeyH" && e.code !== "KeyL")) {
        return false;
      }
      if (shortcut === "alt-hl") {
        return e.altKey && !e.ctrlKey && !e.shiftKey;
      }
      if (shortcut === "ctrl-alt-hl") {
        return (
          (e.ctrlKey && e.altKey && !e.shiftKey) ||
          (e.shiftKey && e.altKey && !e.ctrlKey)
        );
      }
      return e.ctrlKey && !e.altKey && !e.shiftKey;
    };

    const handler = (e: KeyboardEvent) => {
      const target = e.target as HTMLElement | null;
      const isInput =
        !!target &&
        (target.tagName === "INPUT" ||
          target.tagName === "TEXTAREA" ||
          target.isContentEditable);
      const isTerminalInput =
        !!target?.closest(".xterm") ||
        !!target?.classList.contains("xterm-helper-textarea");

      const actions = getActions();
      const mod = IS_MAC ? e.metaKey : e.metaKey || e.ctrlKey;

      // Palette: Cmd+K (Mac) / Ctrl+K (other), works everywhere.
      if (mod && !e.shiftKey && !e.altKey && e.key.toLowerCase() === "k") {
        e.preventDefault();
        e.stopPropagation();
        actions.onPalette();
        return;
      }

      // Toggle terminal focus: Cmd+` (Mac) / Ctrl+` (other), works everywhere.
      // Use e.code so layouts where backtick lives behind a modifier still match.
      // No stopPropagation: preventDefault is enough to suppress the browser's
      // own Cmd+` window cycling, and we don't want to silently shadow other
      // doc-level listeners that might want this combo.
      if (mod && !e.shiftKey && !e.altKey && e.code === "Backquote") {
        e.preventDefault();
        actions.onToggleTerminalFocus();
        return;
      }

      if (
        matchesProjectNavigationShortcut(
          e,
          actions.projectStripShortcut ?? "alt-hl",
        ) &&
        (!isInput || isTerminalInput)
      ) {
        const action =
          e.code === "KeyH" ? actions.onPreviousProject : actions.onNextProject;
        if (action) {
          e.preventDefault();
          e.stopPropagation();
          action();
          return;
        }
      }

      // Use e.code for B shortcuts because Option+B on Mac produces "∫"
      // instead of "b", causing e.key matching to fail.
      // Toggle right panel: Cmd+Opt+B (Mac) / Ctrl+Alt+B (other)
      // Check alt combo first so Cmd+B doesn't swallow Cmd+Opt+B.
      if (mod && !e.shiftKey && e.altKey && e.code === "KeyB") {
        e.preventDefault();
        e.stopPropagation();
        actions.onToggleRightPanel();
        return;
      }

      // Toggle left sidebar: Cmd+B (Mac) / Ctrl+B (other)
      if (mod && !e.shiftKey && !e.altKey && e.code === "KeyB") {
        e.preventDefault();
        e.stopPropagation();
        actions.onToggleSidebar();
        return;
      }

      if (e.key === "Escape") {
        actions.onEscape();
        return;
      }

      if (isInput) return;
      if (e.metaKey || e.ctrlKey || e.altKey) return;

      switch (e.key) {
        case "n":
          e.preventDefault();
          actions.onNew();
          break;
        case "D":
          e.preventDefault();
          actions.onDiff();
          break;
        case "?":
          e.preventDefault();
          actions.onHelp();
          break;
        case "s":
          e.preventDefault();
          actions.onSettings();
          break;
      }
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
