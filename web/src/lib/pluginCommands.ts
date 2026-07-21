// Pure helpers for turning active plugin commands into command-palette actions
// and keybind handlers. Kept side-effect free (except `openExternal`) so the
// resolution and chord-matching rules are unit-tested in one place.

import type { CommandAction } from "../components/command-palette/types";
import { invokePluginCommand, type PluginCommand, type PluginUiEntry } from "./api";
import { reportError } from "./toastBus";

/** Only `http`/`https` URLs may be opened; reject `javascript:`, `file:`,
 *  `data:`, and anything else a plugin might smuggle into an href. */
export function isExternalHttpUrl(u: unknown): u is string {
  return typeof u === "string" && /^https?:\/\//i.test(u);
}

/** Open an external URL in a new tab with the opener relationship severed. */
export function openExternal(url: string): void {
  window.open(url, "_blank", "noopener,noreferrer");
}

/** Dispatch an action-less command to its worker, surfacing a failure as an
 *  error toast so read-only mode, a missing worker, an invalid session, or a
 *  network error is not silently swallowed. Shared by the palette action and
 *  the keybind handler. */
export function invokeActionlessCommand(cmd: PluginCommand, sessionId: string): void {
  void invokePluginCommand(cmd.fqid, sessionId).then((ok) => {
    if (!ok) reportError(`Failed to run ${cmd.title || cmd.id}`);
  });
}

/** One openable link an `open-ui-link` command exposes: a validated href plus a
 *  human label (from the badge item's tooltip/text). */
export interface CommandLink {
  href: string;
  label: string;
}

function entryFor(
  cmd: PluginCommand,
  entries: PluginUiEntry[],
  activeSessionId: string | null,
): PluginUiEntry | undefined {
  if (!activeSessionId || cmd.action?.kind !== "open-ui-link") return undefined;
  const { slot, id } = cmd.action;
  return entries.find(
    (e) => e.plugin_id === cmd.plugin_id && e.slot === slot && e.id === id && e.session_id === activeSessionId,
  );
}

/** Every link an `open-ui-link` command can open for the active session, deduped
 *  by href. A multi-repo workspace exposes one link per open PR via the entry's
 *  `items`; a single-link slot falls back to the entry's top-level `href`. Empty
 *  when there is no active session, no matching entry, or no safe href. */
export function resolveCommandLinks(
  cmd: PluginCommand,
  entries: PluginUiEntry[],
  activeSessionId: string | null,
): CommandLink[] {
  const entry = entryFor(cmd, entries, activeSessionId);
  if (!entry) return [];
  const links: CommandLink[] = [];
  const seen = new Set<string>();
  const push = (href: unknown, label: unknown) => {
    if (!isExternalHttpUrl(href) || seen.has(href)) return;
    seen.add(href);
    links.push({ href, label: typeof label === "string" && label ? label : href });
  };
  const items = entry.payload.items;
  if (Array.isArray(items)) {
    for (const raw of items) {
      // payload is untyped plugin JSON; a primitive or null item must not crash
      // resolution for the whole session.
      if (!raw || typeof raw !== "object") continue;
      const item = raw as Record<string, unknown>;
      push(item.href, item.tooltip ?? item.text);
    }
  }
  // Fall back to the entry's top-level href (e.g. a single-link slot, or a badge
  // with no per-item hrefs).
  if (links.length === 0) push(entry.payload.href, entry.payload.tooltip ?? entry.payload.text);
  return links;
}

/** Palette entries for the active session's plugin commands. An `open-ui-link`
 *  command with a single link becomes one entry; a multi-repo workspace with
 *  several open PRs becomes one entry per PR so the palette is the picker, and a
 *  command whose links do not resolve is omitted so no dead "open" is shown. An
 *  action-less command (no client `action`) becomes one entry that dispatches
 *  `plugin.command.invoke` to the worker; it needs an active session to scope
 *  the invocation, so it is omitted when there is none. */
export function buildPluginCommandActions(
  commands: PluginCommand[],
  entries: PluginUiEntry[],
  activeSessionId: string | null,
): CommandAction[] {
  const actions: CommandAction[] = [];
  for (const cmd of commands) {
    if (cmd.action?.kind === "open-ui-link") {
      const links = resolveCommandLinks(cmd, entries, activeSessionId);
      const multiple = links.length > 1;
      links.forEach((link, i) => {
        actions.push({
          id: multiple ? `plugin:${cmd.fqid}:${i}` : `plugin:${cmd.fqid}`,
          title: multiple ? `${cmd.title || cmd.id}: ${link.label}` : cmd.title || cmd.id,
          subtitle: multiple ? undefined : cmd.description || undefined,
          group: "Actions",
          keywords: ["plugin", cmd.plugin_id, cmd.id],
          shortcut: !multiple ? cmd.keybinds[0] : undefined,
          perform: () => openExternal(link.href),
        });
      });
    } else if (!cmd.action && activeSessionId) {
      actions.push({
        id: `plugin:${cmd.fqid}`,
        title: cmd.title || cmd.id,
        subtitle: cmd.description || undefined,
        group: "Actions",
        keywords: ["plugin", cmd.plugin_id, cmd.id],
        shortcut: cmd.keybinds[0],
        perform: () => invokeActionlessCommand(cmd, activeSessionId),
      });
    }
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

/** What a matched plugin keybind should do: open a single resolved URL, show a
 *  numbered picker for several (`open-ui-link` in a multi-repo workspace), or
 *  dispatch an action-less command to its worker. */
export type KeybindEffect =
  | { kind: "open"; href: string }
  | { kind: "pick"; links: CommandLink[] }
  | { kind: "invoke"; cmd: PluginCommand };

/** The effect for a keydown event: the first command whose chord matches AND
 *  can execute. An `open-ui-link` chord opens its one link, or shows a numbered
 *  picker when the session has several; a chord with no resolvable link is
 *  skipped so a second command sharing the chord still fires. An action-less
 *  chord needs an active session to invoke in. `null` when nothing matches or
 *  nothing is executable. */
export function pickKeybindEffect(
  commands: PluginCommand[],
  entries: PluginUiEntry[],
  activeSessionId: string | null,
  e: KeyboardEvent,
): KeybindEffect | null {
  for (const cmd of commands) {
    for (const key of cmd.keybinds) {
      const chord = parsePluginChord(key);
      if (!chord || !matchPluginChord(chord, e)) continue;
      if (cmd.action?.kind === "open-ui-link") {
        const links = resolveCommandLinks(cmd, entries, activeSessionId);
        if (links.length === 1) return { kind: "open", href: links[0]!.href };
        if (links.length > 1) return { kind: "pick", links };
      } else if (!cmd.action && activeSessionId) {
        return { kind: "invoke", cmd };
      }
    }
  }
  return null;
}
