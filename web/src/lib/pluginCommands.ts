// Pure helpers for turning active plugin commands into command-palette actions
// and keybind handlers. Kept side-effect free (except `openExternal`) so the
// resolution and chord-matching rules are unit-tested in one place.

import type { CommandAction } from "../components/command-palette/types";
import type { PluginCommand, PluginUiEntry } from "./api";

/** Only `http`/`https` URLs may be opened; reject `javascript:`, `file:`,
 *  `data:`, and anything else a plugin might smuggle into an href. */
export function isExternalHttpUrl(u: unknown): u is string {
  return typeof u === "string" && /^https?:\/\//i.test(u);
}

/** Open an external URL in a new tab with the opener relationship severed. */
export function openExternal(url: string): void {
  window.open(url, "_blank", "noopener,noreferrer");
}

/** The href an `open-ui-link` command would open for the active session: the
 *  `href` on the plugin's own `(slot, id)` UI-state entry, validated. `null`
 *  when there is no active session, no matching entry, or no safe href (e.g. the
 *  session has no open PR). */
export function resolveCommandHref(
  cmd: PluginCommand,
  entries: PluginUiEntry[],
  activeSessionId: string | null,
): string | null {
  if (!activeSessionId || cmd.action?.kind !== "open-ui-link") return null;
  const { slot, id } = cmd.action;
  const entry = entries.find(
    (e) => e.plugin_id === cmd.plugin_id && e.slot === slot && e.id === id && e.session_id === activeSessionId,
  );
  const href = entry?.payload.href;
  return isExternalHttpUrl(href) ? href : null;
}

/** Palette entries for the active session's client-executable plugin commands.
 *  An `open-ui-link` command is shown only when its href resolves, so the
 *  palette never offers a dead "open" with nothing to open. */
export function buildPluginCommandActions(
  commands: PluginCommand[],
  entries: PluginUiEntry[],
  activeSessionId: string | null,
): CommandAction[] {
  const actions: CommandAction[] = [];
  for (const cmd of commands) {
    if (cmd.action?.kind !== "open-ui-link") continue;
    const href = resolveCommandHref(cmd, entries, activeSessionId);
    if (!href) continue;
    actions.push({
      id: `plugin:${cmd.fqid}`,
      title: cmd.title || cmd.id,
      subtitle: cmd.description || undefined,
      group: "Actions",
      keywords: ["plugin", cmd.plugin_id, cmd.id],
      shortcut: cmd.keybinds[0],
      perform: () => openExternal(href),
    });
  }
  return actions;
}

/** A parsed key chord. Mirrors the host's `parse_chord` set (`Ctrl`/`Shift`
 *  plus a base key); `Alt`/`Meta` are tolerated here for forward compatibility
 *  even though the TUI rejects them. */
export interface ParsedChord {
  ctrl: boolean;
  shift: boolean;
  alt: boolean;
  meta: boolean;
  base: string;
}

/** Parse a chord string like `Ctrl+Shift+G` into modifiers plus a lowercased
 *  base key, or `null` when it has no base key or two base keys. */
export function parsePluginChord(key: string): ParsedChord | null {
  let ctrl = false;
  let shift = false;
  let alt = false;
  let meta = false;
  let base: string | null = null;
  for (const tok of key
    .split("+")
    .map((t) => t.trim())
    .filter(Boolean)) {
    switch (tok.toLowerCase()) {
      case "ctrl":
      case "control":
        ctrl = true;
        break;
      case "shift":
        shift = true;
        break;
      case "alt":
      case "option":
        alt = true;
        break;
      case "meta":
      case "cmd":
      case "super":
        meta = true;
        break;
      default:
        if (base !== null) return null;
        base = tok.toLowerCase();
    }
  }
  return base ? { ctrl, shift, alt, meta, base } : null;
}

/** Whether a keydown event matches a parsed chord exactly (every modifier and
 *  the base key). */
export function matchPluginChord(chord: ParsedChord, e: KeyboardEvent): boolean {
  return (
    e.ctrlKey === chord.ctrl &&
    e.shiftKey === chord.shift &&
    e.altKey === chord.alt &&
    e.metaKey === chord.meta &&
    e.key.toLowerCase() === chord.base
  );
}
