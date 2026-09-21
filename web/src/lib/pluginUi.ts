// Pure selectors over the plugin UI-state snapshot.

import { createElement, forwardRef, useState, type ComponentType, type CSSProperties } from "react";
import type { LucideIcon, LucideProps } from "lucide-react";
import { DynamicIcon, iconNames } from "lucide-react/dynamic";

// Names are validated against `iconNames` before reaching this.
const AnyIcon = DynamicIcon as ComponentType<LucideProps & { name: string }>;

import type { PluginUiEntry, PluginUiSlot, PluginUiTone } from "./api";

// `DynamicIcon` lazy-loads each icon; unknown names resolve to undefined.
const VALID = new Set<string>(iconNames);
const cache = new Map<string, LucideIcon>();

/** Identity is cached per name so the icon does not remount. */
export function lucideIcon(name: string | undefined): LucideIcon | undefined {
  if (!name || !VALID.has(name)) return undefined;
  const hit = cache.get(name);
  if (hit) return hit;
  const Icon = forwardRef<SVGSVGElement, LucideProps>((props, ref) =>
    createElement(AnyIcon, { name, ref, ...props }),
  ) as LucideIcon;
  cache.set(name, Icon);
  return Icon;
}

/** Resets when the URL changes, so a failure on one URL doesn't hide a later working one. */
export function useAssetFailed(url: string | null | undefined): [boolean, () => void] {
  const [failed, setFailed] = useState(false);
  const [trackedUrl, setTrackedUrl] = useState(url);
  if (url !== trackedUrl) {
    setTrackedUrl(url);
    setFailed(false);
  }
  return [failed, () => setFailed(true)];
}

/** `undefined` or unknown tones fall back to neutral. */
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

export function globalEntries(entries: PluginUiEntry[], slot: PluginUiSlot): PluginUiEntry[] {
  return entries.filter((e) => e.slot === slot && e.session_id == null);
}

/** Also the tearing guard: entries for vanished sessions never match. */
export function sessionEntries(
  entries: PluginUiEntry[],
  slot: PluginUiSlot,
  sessionId: string | undefined,
): PluginUiEntry[] {
  if (!sessionId) return [];
  return entries.filter((e) => e.slot === slot && e.session_id === sessionId);
}

export function payloadStr(entry: PluginUiEntry, key: string): string {
  const v = entry.payload[key];
  return typeof v === "string" ? v : "";
}

export function entryText(entry: PluginUiEntry): string {
  return payloadStr(entry, "text");
}

/** Only `#rgb`/`#rrggbb` becomes lowercase `#rrggbb`, so the value can never carry arbitrary CSS. */
export function validColor(v: unknown): string | undefined {
  if (typeof v !== "string") return undefined;
  const short = /^#([0-9a-f]{3})$/i.exec(v)?.[1];
  if (short) {
    return ("#" + short.replace(/./g, (c) => c + c)).toLowerCase();
  }
  return /^#[0-9a-f]{6}$/i.test(v) ? v.toLowerCase() : undefined;
}

/** Only a validated hex reaches fixed color properties, never a class or raw CSS. */
export function accentStyle(color: unknown, withFill = false): CSSProperties | undefined {
  const c = validColor(color);
  if (!c) return undefined;
  return withFill ? { color: c, backgroundColor: `color-mix(in oklab, ${c} 15%, transparent)` } : { color: c };
}

export function validTone(t: unknown): PluginUiTone | undefined {
  if (t === "info" || t === "success" || t === "warn" || t === "danger" || t === "neutral") {
    return t;
  }
  return undefined;
}

export function entryTone(entry: PluginUiEntry): PluginUiTone | undefined {
  return validTone(entry.payload.tone);
}

export function toneTextClass(tone: PluginUiTone | undefined): string {
  return (
    toneClasses(tone)
      .split(" ")
      .find((c) => c.startsWith("text-")) ?? "text-text-dim"
  );
}

// `sort-key` and `filter-facet` are global entries referencing a per-session `row-column` by id; sorting and filtering run client-side.

/** Matches the host's untagged `SortValue`. */
export type PluginSortValue = number | string;

export function entrySortValue(entry: PluginUiEntry): PluginSortValue | undefined {
  const v = entry.payload.sort_value;
  if (typeof v === "number" && Number.isFinite(v)) return v;
  if (typeof v === "string") return v;
  return undefined;
}

export function entryFilterValues(entry: PluginUiEntry): string[] {
  const v = entry.payload.filter_values;
  return Array.isArray(v) ? v.filter((x): x is string => typeof x === "string") : [];
}

/** Missing values sink in both directions; numbers sort before strings when mixed. */
export function compareSortValues(
  a: PluginSortValue | undefined,
  b: PluginSortValue | undefined,
  direction: "asc" | "desc",
): number {
  if (a === undefined && b === undefined) return 0;
  if (a === undefined) return 1;
  if (b === undefined) return -1;
  let cmp: number;
  if (typeof a === "number" && typeof b === "number") cmp = a < b ? -1 : a > b ? 1 : 0;
  else if (typeof a === "string" && typeof b === "string") cmp = a.localeCompare(b);
  else cmp = typeof a === "number" ? -1 : 1;
  return direction === "desc" ? -cmp : cmp;
}

export interface PluginSortSpec {
  pluginId: string;
  entryId: string;
  label: string;
  column: string;
  direction: "asc" | "desc";
}

/** Entries missing a label or column are skipped; direction defaults to ascending. */
export function pluginSortSpecs(entries: PluginUiEntry[]): PluginSortSpec[] {
  const out: PluginSortSpec[] = [];
  for (const e of entries) {
    if (e.slot !== "sort-key" || e.session_id != null) continue;
    const label = payloadStr(e, "label");
    const column = payloadStr(e, "column");
    if (!label || !column) continue;
    out.push({
      pluginId: e.plugin_id,
      entryId: e.id,
      label,
      column,
      direction: e.payload.direction === "desc" ? "desc" : "asc",
    });
  }
  return out;
}

export interface PluginFacetSpec {
  pluginId: string;
  entryId: string;
  label: string;
  column: string;
  options: { value: string; label: string; tone: PluginUiTone | undefined }[];
}

/** Entries missing a label or column, and options missing a value, are skipped. */
export function pluginFacetSpecs(entries: PluginUiEntry[]): PluginFacetSpec[] {
  const out: PluginFacetSpec[] = [];
  for (const e of entries) {
    if (e.slot !== "filter-facet" || e.session_id != null) continue;
    const label = payloadStr(e, "label");
    const column = payloadStr(e, "column");
    if (!label || !column) continue;
    const raw = Array.isArray(e.payload.options) ? e.payload.options : [];
    const options = raw
      .filter((o): o is Record<string, unknown> => typeof o === "object" && o !== null && !Array.isArray(o))
      .map((o) => ({
        value: typeof o.value === "string" ? o.value : "",
        label: typeof o.label === "string" && o.label ? o.label : typeof o.value === "string" ? o.value : "",
        tone: validTone(o.tone),
      }))
      .filter((o) => o.value !== "");
    out.push({ pluginId: e.plugin_id, entryId: e.id, label, column, options });
  }
  return out;
}

/** Sessions without a comparable scalar are omitted and sink. */
export function buildSortValueMap(
  entries: PluginUiEntry[],
  pluginId: string,
  column: string,
): Map<string, PluginSortValue> {
  const map = new Map<string, PluginSortValue>();
  for (const e of entries) {
    if (e.slot !== "row-column" || e.plugin_id !== pluginId || e.id !== column || e.session_id == null) continue;
    const v = entrySortValue(e);
    if (v !== undefined) map.set(e.session_id, v);
  }
  return map;
}

export interface ActiveFacet {
  pluginId: string;
  column: string;
  values: Set<string>;
}

/** AND across facets, OR within one; a session without the column fails that facet. */
export function sessionMatchesFacets(entries: PluginUiEntry[], sessionId: string, active: ActiveFacet[]): boolean {
  return active.every((f) => {
    const rc = entries.find(
      (e) => e.slot === "row-column" && e.plugin_id === f.pluginId && e.id === f.column && e.session_id === sessionId,
    );
    if (!rc) return false;
    return entryFilterValues(rc).some((v) => f.values.has(v));
  });
}
