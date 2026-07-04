// Pure selectors over the plugin UI-state snapshot (#2366). Components read
// slots through these so the filtering rules (and the per-session tearing
// guard) live in one tested place rather than scattered across the UI.

import { createElement, forwardRef, useState, type ComponentType, type CSSProperties } from "react";
import type { LucideIcon, LucideProps } from "lucide-react";
import { DynamicIcon, iconNames } from "lucide-react/dynamic";

// DynamicIcon types `name` as the full kebab-name union; plugins hand us a
// runtime string we have already validated against `iconNames`, so widen it.
const AnyIcon = DynamicIcon as ComponentType<LucideProps & { name: string }>;

import type { PluginUiEntry, PluginUiSlot, PluginUiTone } from "./api";

// Plugins name an icon by its lucide kebab name (badge items, pane chrome, etc.).
// Any lucide icon is fair game: `DynamicIcon` code-splits each one into its own
// lazy chunk, so the whole barrel never lands in the main bundle. We validate
// against `iconNames` (lucide's own list) so an unknown name resolves to
// undefined and each call site picks its own fallback, rather than rendering
// lucide's missing-icon placeholder.
const VALID = new Set<string>(iconNames);
const cache = new Map<string, LucideIcon>();

/** Resolve a lucide kebab name to a renderable icon component, or undefined for
 *  an empty/unknown name. The component lazy-loads its icon; identity is cached
 *  per name so it does not remount each render. */
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

/** Tracks whether an image URL has failed to load, resetting automatically
 *  when the URL itself changes (e.g. a fallback route getting replaced by a
 *  resolved manifest URL once a fetch lands), so a transient failure on one
 *  URL doesn't permanently hide a later working one. Shared by every
 *  identity-icon renderer (`PluginIdentityIcon`, the activity-bar/dock pane
 *  icon) so the reset-on-change logic lives in one tested place. */
export function useAssetFailed(url: string | null | undefined): [boolean, () => void] {
  const [failed, setFailed] = useState(false);
  const [trackedUrl, setTrackedUrl] = useState(url);
  if (url !== trackedUrl) {
    setTrackedUrl(url);
    setFailed(false);
  }
  return [failed, () => setFailed(true)];
}

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

/** Validate a plugin-supplied color to a normalized lowercase `#rrggbb`, or
 *  undefined for anything else. Only `#rgb`/`#rrggbb` hex literals are accepted:
 *  no CSS names, `rgb()`, `var()`, or `url()`, so the value can never carry
 *  arbitrary CSS. The closed `tone` set stays the semantic axis; `color` is an
 *  optional literal accent the worker uses where a tone cannot name the hue
 *  (e.g. a merged PR's purple). */
export function validColor(v: unknown): string | undefined {
  if (typeof v !== "string") return undefined;
  const short = /^#([0-9a-f]{3})$/i.exec(v)?.[1];
  if (short) {
    return ("#" + short.replace(/./g, (c) => c + c)).toLowerCase();
  }
  return /^#[0-9a-f]{6}$/i.test(v) ? v.toLowerCase() : undefined;
}

/** A React style object applying a validated `color` as the foreground tint,
 *  with a theme-aware translucent fill when `withFill` is set (for pills). The
 *  hex reaches the DOM only as the value of fixed color properties, never as a
 *  class or raw CSS, so the host's no-arbitrary-CSS guarantee holds. Returns
 *  undefined when the color is absent/invalid, so the caller falls back to its
 *  tone classes.
 *
 *  Trust boundary: `validColor` is the ONLY thing standing between plugin input
 *  and the `color-mix(...)` CSS string below. It must keep rejecting anything
 *  that is not a bare `#rrggbb` hex; never loosen it without re-auditing this
 *  interpolation, or the fill becomes a CSS-injection vector. */
export function accentStyle(color: unknown, withFill = false): CSSProperties | undefined {
  const c = validColor(color);
  if (!c) return undefined;
  return withFill ? { color: c, backgroundColor: `color-mix(in oklab, ${c} 15%, transparent)` } : { color: c };
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

// `sort-key` and `filter-facet` (#2401) are global entries that reference a
// per-session `row-column` entry by its `id` (the payload `column` field). The dashboard
// orders/filters session rows client-side over the already-fetched scalars; no
// plugin code runs and the render path never awaits a worker. Lookups are
// scoped to the referencing plugin's own `plugin_id`, since a `column` id is
// only unique within one plugin.

/** A comparable scalar a `row-column` exposes for client-side sorting, matching
 *  the host's untagged `SortValue` (number | string). */
export type PluginSortValue = number | string;

/** A `row-column`'s `sort_value`, validated to a finite number or a string;
 *  undefined when absent or not a comparable scalar. */
export function entrySortValue(entry: PluginUiEntry): PluginSortValue | undefined {
  const v = entry.payload.sort_value;
  if (typeof v === "number" && Number.isFinite(v)) return v;
  if (typeof v === "string") return v;
  return undefined;
}

/** A `row-column`'s `filter_values`, as the string tokens it matches. */
export function entryFilterValues(entry: PluginUiEntry): string[] {
  const v = entry.payload.filter_values;
  return Array.isArray(v) ? v.filter((x): x is string => typeof x === "string") : [];
}

/** Compare two scalar sort values for a direction. Missing values (undefined)
 *  always sort to the bottom regardless of direction, so an unvalued row sinks.
 *  Numbers compare numerically, strings via `localeCompare`; when the two are
 *  mixed a number sorts before a string, so the order stays deterministic.
 *  Returns a negative number when `a` should rank before `b`. */
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

/** A renderable plugin sort option, resolved from a global `sort-key` entry. */
export interface PluginSortSpec {
  pluginId: string;
  entryId: string;
  label: string;
  /** The `row-column` id whose `sort_value` this orders by. */
  column: string;
  direction: "asc" | "desc";
}

/** Global `sort-key` entries as renderable sort options, in snapshot order.
 *  Entries missing a label or column are skipped; an omitted direction defaults
 *  to ascending. */
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

/** A renderable facet control, resolved from a global `filter-facet` entry. */
export interface PluginFacetSpec {
  pluginId: string;
  entryId: string;
  label: string;
  /** The `row-column` id whose `filter_values` this filters over. */
  column: string;
  options: { value: string; label: string; tone: PluginUiTone | undefined }[];
}

/** Global `filter-facet` entries as renderable facet controls, in snapshot
 *  order. Entries missing a label or column, and options missing a value, are
 *  skipped. */
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

/** Map of `session_id` -> `sort_value` for one plugin's `row-column` id, for
 *  the active sort. Sessions whose entry carries no comparable scalar are
 *  omitted (they sink to the bottom at compare time). */
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

/** One active facet selection: the referencing plugin/column plus the chosen
 *  values (OR within this set). */
export interface ActiveFacet {
  pluginId: string;
  column: string;
  values: Set<string>;
}

/** Whether a session satisfies every active facet: for each facet the
 *  session's matching `row-column` entry shares at least one `filter_value`
 *  with the facet's selected set. AND across facets, OR within one facet. A
 *  session with no matching row-column for an active facet fails that facet. */
export function sessionMatchesFacets(entries: PluginUiEntry[], sessionId: string, active: ActiveFacet[]): boolean {
  return active.every((f) => {
    const rc = entries.find(
      (e) => e.slot === "row-column" && e.plugin_id === f.pluginId && e.id === f.column && e.session_id === sessionId,
    );
    if (!rc) return false;
    return entryFilterValues(rc).some((v) => f.values.has(v));
  });
}
