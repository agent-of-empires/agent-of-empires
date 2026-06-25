// Pure selectors over the plugin UI-state snapshot (#2366). Components read
// slots through these so the filtering rules (and the per-session tearing
// guard) live in one tested place rather than scattered across the UI.

import type { PluginUiEntry, PluginUiSlot, PluginUiTone } from "./api";

/** Tailwind classes per tone, shared by every slot renderer so a plugin's
 *  tone maps to one consistent palette. `undefined`/unknown falls back to
 *  neutral. */
export function toneClasses(tone: PluginUiTone | undefined): string {
  switch (tone) {
    case "info":
      return "bg-sky-500/15 text-sky-300";
    case "success":
      return "bg-emerald-500/15 text-emerald-300";
    case "warn":
      return "bg-amber-500/15 text-amber-300";
    case "danger":
      return "bg-rose-500/15 text-rose-300";
    default:
      return "bg-slate-500/15 text-slate-300";
  }
}

/** Global (non per-session) entries for a slot, in snapshot order. */
export function globalEntries(entries: PluginUiEntry[], slot: PluginUiSlot): PluginUiEntry[] {
  return entries.filter((e) => e.slot === slot && e.session_id == null);
}

/** Per-session entries for a slot scoped to one session. A null/absent
 *  `sessionId` yields nothing; this is also the tearing guard, since callers
 *  pass a live session id and entries for vanished sessions never match. */
export function sessionEntries(
  entries: PluginUiEntry[],
  slot: PluginUiSlot,
  sessionId: string | undefined,
): PluginUiEntry[] {
  if (!sessionId) return [];
  return entries.filter((e) => e.slot === slot && e.session_id === sessionId);
}

/** A row-column entry's string text, if present. */
export function entryText(entry: PluginUiEntry): string {
  const text = entry.payload.text;
  return typeof text === "string" ? text : "";
}

/** A row-column entry's sort scalar (number or string), for client-side
 *  ordering. `undefined` sorts last. */
export function entrySortValue(entry: PluginUiEntry): number | string | undefined {
  const v = entry.payload.sort_value;
  if (typeof v === "number" || typeof v === "string") return v;
  return undefined;
}

/** Compare two optional sort scalars; undefined sorts after defined values.
 *  Numbers compare numerically, strings lexically, mixed by type name. */
export function compareSortValues(a: number | string | undefined, b: number | string | undefined): number {
  if (a === undefined && b === undefined) return 0;
  if (a === undefined) return 1;
  if (b === undefined) return -1;
  if (typeof a === "number" && typeof b === "number") return a - b;
  return String(a).localeCompare(String(b));
}
