// Pure selectors over the plugin UI-state snapshot (#2366). Components read
// slots through these so the filtering rules (and the per-session tearing
// guard) live in one tested place rather than scattered across the UI.

import type { PluginUiEntry, PluginUiSlot, PluginUiTone } from "./api";

/** Theme-backed classes per tone, shared by every slot renderer so a plugin's
 *  tone maps to one consistent palette that repaints with the user's theme
 *  (the `status-*` colors are CSS-variable backed). `undefined`/unknown falls
 *  back to neutral. */
export function toneClasses(tone: PluginUiTone | undefined): string {
  switch (tone) {
    case "info":
      return "bg-status-unread/15 text-status-unread";
    case "success":
      return "bg-status-running/15 text-status-running";
    case "warn":
      return "bg-status-waiting/15 text-status-waiting";
    case "danger":
      return "bg-status-error/15 text-status-error";
    default:
      return "bg-status-idle/15 text-status-idle";
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

/** A string field of an entry's payload, or "" when absent/non-string. */
export function payloadStr(entry: PluginUiEntry, key: string): string {
  const v = entry.payload[key];
  return typeof v === "string" ? v : "";
}

/** An entry's primary `text` field. */
export function entryText(entry: PluginUiEntry): string {
  return payloadStr(entry, "text");
}

/** Validate an arbitrary value against the closed tone set (used for badge
 *  items and detail blocks where the tone is nested, not on the entry). */
export function validTone(t: unknown): PluginUiTone | undefined {
  if (t === "info" || t === "success" || t === "warn" || t === "danger" || t === "neutral") {
    return t;
  }
  return undefined;
}

/** An entry's optional `tone`, validated to the closed set (anything else
 *  reads as neutral). */
export function entryTone(entry: PluginUiEntry): PluginUiTone | undefined {
  return validTone(entry.payload.tone);
}

/** Just the `text-*` color class for a tone, for surfaces that tint text/icons
 *  without a filled background (row columns, detail rows). */
export function toneTextClass(tone: PluginUiTone | undefined): string {
  return (
    toneClasses(tone)
      .split(" ")
      .find((c) => c.startsWith("text-")) ?? "text-text-dim"
  );
}
