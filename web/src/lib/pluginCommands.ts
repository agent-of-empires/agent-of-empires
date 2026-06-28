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

/** The primary href an `open-ui-link` command opens (for the keybind, which
 *  cannot disambiguate): the entry's top-level `href` if valid, else the first
 *  resolved link. `null` when nothing safe resolves. */
export function resolveCommandHref(
  cmd: PluginCommand,
  entries: PluginUiEntry[],
  activeSessionId: string | null,
): string | null {
  const entry = entryFor(cmd, entries, activeSessionId);
  const top = entry?.payload.href;
  if (isExternalHttpUrl(top)) return top;
  return resolveCommandLinks(cmd, entries, activeSessionId)[0]?.href ?? null;
}

/** Palette entries for the active session's client-executable plugin commands.
 *  A command with a single link becomes one entry; a multi-repo workspace with
 *  several open PRs becomes one entry per PR so the palette is the picker. A
 *  command whose links do not resolve is omitted, so no dead "open" is shown. */
export function buildPluginCommandActions(
  commands: PluginCommand[],
  entries: PluginUiEntry[],
  activeSessionId: string | null,
): CommandAction[] {
  const actions: CommandAction[] = [];
  for (const cmd of commands) {
    if (cmd.action?.kind !== "open-ui-link") continue;
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

/** The href to open for a keydown event: the first command whose chord matches
 *  AND whose primary href resolves. A chord match that can't execute (no PR for
 *  this session) is skipped, so a second command sharing the chord still fires.
 *  `null` when nothing matches or nothing resolves. */
export function pickKeybindHref(
  commands: PluginCommand[],
  entries: PluginUiEntry[],
  activeSessionId: string | null,
  e: KeyboardEvent,
): string | null {
  for (const cmd of commands) {
    if (cmd.action?.kind !== "open-ui-link") continue;
    for (const key of cmd.keybinds) {
      const chord = parsePluginChord(key);
      if (!chord || !matchPluginChord(chord, e)) continue;
      const href = resolveCommandHref(cmd, entries, activeSessionId);
      if (href) return href;
    }
  }
  return null;
}
