// Pure helpers turning plugin commands into palette actions and keybind handlers.

import type { CommandAction } from "../components/command-palette/types";
import { invokePluginCommand, type PluginCommand, type PluginUiEntry } from "./api";
import { isAllowedHref, isInternalHref, navigateInternalHref } from "./pluginHref";
import { reportError } from "./toastBus";

/** A same-origin href navigates via the router; anything else opens a new tab. */
export function openPluginLink(href: string): void {
  if (isInternalHref(href)) {
    navigateInternalHref(href);
  } else {
    window.open(href, "_blank", "noopener,noreferrer");
  }
}

/** Surfaces failures (read-only, no worker, network) as an error toast. */
export function invokeActionlessCommand(cmd: PluginCommand, sessionId: string): void {
  void invokePluginCommand(cmd.fqid, sessionId).then((ok) => {
    if (!ok) reportError(`Failed to run ${cmd.title || cmd.id}`);
  });
}

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

/** Links for the active session's `open-ui-link` command, deduped by href: one per item, else the entry's top-level href. */
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
    if (!isAllowedHref(href) || seen.has(href)) return;
    seen.add(href);
    links.push({ href, label: typeof label === "string" && label ? label : href });
  };
  const items = entry.payload.items;
  if (Array.isArray(items)) {
    for (const raw of items) {
      // Plugin payloads are untyped; skip primitive or null items.
      if (!raw || typeof raw !== "object") continue;
      const item = raw as Record<string, unknown>;
      push(item.href, item.tooltip ?? item.text);
    }
  }
  if (links.length === 0) push(entry.payload.href, entry.payload.tooltip ?? entry.payload.text);
  return links;
}

/** One entry per resolvable link (so the palette is the picker), omitting commands without one; action-less commands get a single invoke entry. */
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
          perform: () => openPluginLink(link.href),
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

/** Mirrors the host's `parse_chord`; `Alt`/`Meta` are tolerated for forward compatibility. */
export interface ParsedChord {
  ctrl: boolean;
  shift: boolean;
  alt: boolean;
  meta: boolean;
  base: string;
}

/** Null without exactly one base key. */
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

export function matchPluginChord(chord: ParsedChord, e: KeyboardEvent): boolean {
  return (
    e.ctrlKey === chord.ctrl &&
    e.shiftKey === chord.shift &&
    e.altKey === chord.alt &&
    e.metaKey === chord.meta &&
    e.key.toLowerCase() === chord.base
  );
}

export type KeybindEffect =
  | { kind: "open"; href: string }
  | { kind: "pick"; links: CommandLink[] }
  | { kind: "invoke"; cmd: PluginCommand };

/** The first matching command that can execute; a chord without a resolvable link falls through to the next command sharing it. */
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
