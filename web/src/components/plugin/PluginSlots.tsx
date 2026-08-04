// Renderers for the host-rendered plugin UI slots (#2366). The host ships
// typed display state; these components draw it. No plugin code runs here.
// Each reads the shared snapshot via context and the pure selectors in
// `pluginUi.ts`. Slots shipped here: status-bar, row-badge, row-column, card,
// pane, detail-badge, composer-action, settings-page. Notifications surface as toasts via the hook; the
// sort-key and filter-facet slots render as sidebar sort options and a facet
// filter (the sidebar owns those; see SidebarSortPicker / WorkspaceSidebar, #2401).

import { createElement, useEffect, useId, useRef, useState } from "react";
import { ArrowUpRight, ChevronRight } from "lucide-react";

import { invokePluginAction } from "../../lib/api";
import {
  usePluginUiEntries,
  usePluginUiPoke,
  usePluginUiRefreshing,
  usePluginUiRevision,
} from "../../lib/pluginUiContext";
import {
  accentStyle,
  entryText,
  entryTone,
  globalEntries,
  lucideIcon,
  payloadStr,
  sessionEntries,
  toneClasses,
  toneTextClass,
  validTone,
} from "../../lib/pluginUi";
import type { PluginUiEntry, PluginUiTone } from "../../lib/api";

export interface ComposerActionSnapshot {
  text: string;
  selectionStart: number;
  selectionEnd: number;
}

// Plugin strings are untrusted: only follow http/https hrefs, never
// javascript:/data: and friends. Returns undefined for anything else, so the
// badge/row renders as plain text instead of a link.
function safeHref(href: string | undefined): string | undefined {
  return href && /^https?:\/\//i.test(href) ? href : undefined;
}

function isObject(v: unknown): v is Record<string, unknown> {
  return typeof v === "object" && v !== null && !Array.isArray(v);
}

function str(obj: Record<string, unknown>, key: string): string | undefined {
  const v = obj[key];
  return typeof v === "string" ? v : undefined;
}

/** Objects in a payload's `items`/`blocks` array, or undefined when absent. */
function objectList(payload: Record<string, unknown>, key: string): Record<string, unknown>[] | undefined {
  const v = payload[key];
  return Array.isArray(v) ? v.filter(isObject) : undefined;
}

/** A block's `params`: the argument object forwarded verbatim with its action so
 *  the worker knows which subject was acted on (which PR a row selects, which
 *  group a button folds). It round-trips values the plugin itself authored, so
 *  there is nothing to sanitize; the host still merges in the authoritative
 *  `session_id` server-side, which a plugin cannot spoof from here. */
function actionParams(block: Record<string, unknown>): Record<string, unknown> | undefined {
  return isObject(block.params) ? block.params : undefined;
}

/** One pill: an optional tone-tinted icon plus optional text, wrapped in a
 *  link when the href is a safe http(s) URL. Shared by the single-badge slots
 *  and each entry in a `row-badge` `items` list. */
function BadgeChip({
  text,
  icon,
  tone,
  href,
  tooltip,
  slot,
  pluginId,
}: {
  text?: string;
  icon?: string;
  tone?: PluginUiTone;
  href?: string;
  tooltip?: string;
  slot: string;
  pluginId: string;
}) {
  const iconComp = lucideIcon(icon);
  if (!iconComp && !text) return null;
  const safe = safeHref(href);
  // Truncation is only for text badges; an icon-only badge must size to its
  // icon. Without this guard `truncate` (overflow-hidden) + `min-w-0` let the
  // row's flex squeeze the chip and clip the icon (it overflowed to the right).
  const fit = text ? "max-w-48 min-w-0 truncate" : "shrink-0";
  const className = `inline-flex items-center gap-1 font-mono text-[11px] px-1.5 py-0.5 rounded-full ${fit} ${toneClasses(tone)}`;
  const inner = (
    <>
      {iconComp && createElement(iconComp, { className: "size-3 shrink-0", "aria-hidden": true })}
      {text && <span className="truncate">{text}</span>}
    </>
  );
  const common = {
    className,
    title: tooltip || text || undefined,
    // An icon-only badge has no visible text, so `title` alone leaves the link
    // unlabeled for assistive tech: give it an explicit name from the tooltip.
    "aria-label": text ? undefined : tooltip || undefined,
    "data-plugin-slot": slot,
    "data-plugin-id": pluginId,
  };
  if (safe) {
    return (
      <a {...common} href={safe} target="_blank" rel="noopener noreferrer">
        {inner}
      </a>
    );
  }
  return <span {...common}>{inner}</span>;
}

function Badge({ entry }: { entry: PluginUiEntry }) {
  return (
    <BadgeChip
      text={entryText(entry) || undefined}
      icon={payloadStr(entry, "icon") || undefined}
      tone={entryTone(entry)}
      href={payloadStr(entry, "href") || undefined}
      tooltip={payloadStr(entry, "tooltip") || undefined}
      slot={entry.slot}
      pluginId={entry.plugin_id}
    />
  );
}

/** status-bar: global segments in the top bar's right zone. */
export function PluginStatusBarSegments() {
  const entries = globalEntries(usePluginUiEntries(), "status-bar");
  if (entries.length === 0) return null;
  return (
    <>
      {entries.map((e) => (
        <Badge key={`${e.plugin_id}:${e.id}`} entry={e} />
      ))}
    </>
  );
}

/** row-badge: per-session badges on a session row. An entry is either a single
 *  badge (`{ text, tone, icon, href, tooltip }`) or a list (`items: BadgeItem[]`)
 *  so one entry can show several icon badges. An empty `items: []` clears the
 *  row (renders nothing). */
export function PluginRowBadges({ sessionId }: { sessionId: string }) {
  const entries = sessionEntries(usePluginUiEntries(), "row-badge", sessionId);
  if (entries.length === 0) return null;
  return (
    <>
      {entries.map((e) => {
        const items = objectList(e.payload, "items");
        if (items) {
          return items.map((it, i) => (
            <BadgeChip
              key={`${e.plugin_id}:${e.id}:${i}`}
              text={str(it, "text")}
              icon={str(it, "icon")}
              tone={validTone(it.tone)}
              href={str(it, "href")}
              tooltip={str(it, "tooltip")}
              slot="row-badge"
              pluginId={e.plugin_id}
            />
          ));
        }
        return <Badge key={`${e.plugin_id}:${e.id}`} entry={e} />;
      })}
    </>
  );
}

/** row-column: per-session text column, anchored to the right of the plugin
 *  row line (#2514) so it gives up no width to the badges beside it. The
 *  payload may also carry `sort_value` / `filter_values` scalars, which the
 *  sidebar's sort-key and filter-facet controls consume (#2401); this renders
 *  only the visible text. */
export function PluginRowColumn({ sessionId }: { sessionId: string }) {
  const entries = sessionEntries(usePluginUiEntries(), "row-column", sessionId);
  if (entries.length === 0) return null;
  return (
    <span className="ml-auto flex shrink-0 items-center gap-1.5">
      {entries.map((e) => {
        const text = entryText(e);
        if (!text) return null;
        return (
          <span
            key={`${e.plugin_id}:${e.id}`}
            className={`max-w-32 truncate font-mono text-[11px] ${
              toneClasses(entryTone(e))
                .split(" ")
                .find((c) => c.startsWith("text-")) ?? "text-text-dim"
            }`}
            title={payloadStr(e, "tooltip") || text}
            data-plugin-slot="row-column"
            data-plugin-id={e.plugin_id}
          >
            {text}
          </span>
        );
      })}
    </span>
  );
}

/** The plugin row line: badges (wrapping, left) plus the right-anchored
 *  status column, on their own line under the session name (#2514). Keeping
 *  these off the name line stops the narrow mobile sidebar from squeezing the
 *  column text to zero and pushing the badge icons past the drawer edge.
 *  Renders nothing when the session has neither, so plugin-free rows keep their
 *  original height. */
export function PluginRowLine({ sessionId }: { sessionId: string }) {
  const entries = usePluginUiEntries();
  const hasBadges = sessionEntries(entries, "row-badge", sessionId).length > 0;
  const hasColumn = sessionEntries(entries, "row-column", sessionId).length > 0;
  if (!hasBadges && !hasColumn) return null;
  return (
    <span className="mt-0.5 flex items-center gap-1.5">
      <span className="flex min-w-0 flex-wrap items-center gap-1.5">
        <PluginRowBadges sessionId={sessionId} />
      </span>
      <PluginRowColumn sessionId={sessionId} />
    </span>
  );
}

/** card: global cards on the dashboard overview. */
export function PluginCards() {
  const entries = globalEntries(usePluginUiEntries(), "card");
  if (entries.length === 0) return null;
  return (
    <div
      className="mt-4 w-full max-w-2xl grid grid-cols-1 sm:grid-cols-2 lg:grid-cols-3 gap-3"
      data-testid="plugin-cards"
    >
      {entries.map((e) => {
        const title = payloadStr(e, "title");
        const body = payloadStr(e, "body");
        return (
          <div
            key={`${e.plugin_id}:${e.id}`}
            className={`rounded-lg p-3 ring-1 ring-surface-700/60 ${toneClasses(entryTone(e))}`}
            data-plugin-id={e.plugin_id}
          >
            <div className="font-semibold text-sm">{title}</div>
            {body && <div className="mt-1 text-xs text-text-secondary whitespace-pre-wrap">{body}</div>}
          </div>
        );
      })}
    </div>
  );
}

/** detail-badge: per-session badges in the session detail panel. */
export function PluginDetailBadges({ sessionId }: { sessionId: string }) {
  const entries = sessionEntries(usePluginUiEntries(), "detail-badge", sessionId);
  if (entries.length === 0) return null;
  return (
    <div className="flex flex-wrap items-center gap-1.5" data-testid="plugin-detail-badges">
      {entries.map((e) => (
        <Badge key={`${e.plugin_id}:${e.id}`} entry={e} />
      ))}
    </div>
  );
}

/** tool-card-badge: per-session pills on a transcript tool-call card, matched to
 *  the card by its target `(kind, name)`. One entry carries an `items` list so a
 *  plugin can badge every MCP server / skill it knows in one push; this renders
 *  only the items whose target matches this card. Renders nothing when no plugin
 *  badges this target, so a card is byte-identical without a plugin. */
export function PluginToolCardBadges({
  sessionId,
  kind,
  target,
}: {
  sessionId: string;
  kind: "mcp" | "skill";
  target: string;
}) {
  const entries = sessionEntries(usePluginUiEntries(), "tool-card-badge", sessionId);
  if (entries.length === 0) return null;
  const chips = entries.flatMap((e) => {
    const items = objectList(e.payload, "items");
    if (!items) return [];
    return items
      .filter((it) => {
        const t = it.target;
        return isObject(t) && str(t, "kind") === kind && str(t, "name") === target;
      })
      .map((it, i) => (
        <BadgeChip
          key={`${e.plugin_id}:${e.id}:${i}`}
          text={str(it, "text")}
          icon={str(it, "icon")}
          tone={validTone(it.tone)}
          tooltip={str(it, "tooltip")}
          slot="tool-card-badge"
          pluginId={e.plugin_id}
        />
      ));
  });
  if (chips.length === 0) return null;
  return <span className="flex shrink-0 items-center gap-1">{chips}</span>;
}

export function PluginComposerActions({
  sessionId,
  getSnapshot,
}: {
  sessionId: string;
  getSnapshot: () => ComposerActionSnapshot;
}) {
  const entries = sessionEntries(usePluginUiEntries(), "composer-action", sessionId);
  if (entries.length === 0) return null;
  return (
    <>
      {entries.map((entry) => (
        <PluginComposerActionButton
          key={`${entry.plugin_id}:${entry.id}`}
          entry={entry}
          sessionId={sessionId}
          getSnapshot={getSnapshot}
        />
      ))}
    </>
  );
}

function PluginComposerActionButton({
  entry,
  sessionId,
  getSnapshot,
}: {
  entry: PluginUiEntry;
  sessionId: string;
  getSnapshot: () => ComposerActionSnapshot;
}) {
  const label = payloadStr(entry, "label");
  const method = payloadStr(entry, "method");
  const tooltip = payloadStr(entry, "tooltip") || label;
  const iconComp = lucideIcon(payloadStr(entry, "icon") || undefined);
  const disabled = entry.payload.disabled === true || !method || !label;
  const [posting, setPosting] = useState(false);
  const postingRef = useRef(false);
  const poke = usePluginUiPoke();
  if (!label || !method) return null;
  const onClick = async () => {
    if (postingRef.current || disabled) return;
    postingRef.current = true;
    setPosting(true);
    try {
      const snapshot = getSnapshot();
      const accepted = await invokePluginAction(entry.plugin_id, method, sessionId, {
        composer: {
          text: snapshot.text,
          selection_start: snapshot.selectionStart,
          selection_end: snapshot.selectionEnd,
        },
      });
      if (accepted) poke();
    } finally {
      postingRef.current = false;
      setPosting(false);
    }
  };
  return (
    <button
      type="button"
      onClick={onClick}
      disabled={disabled || posting}
      title={tooltip}
      aria-label={label}
      aria-busy={posting || undefined}
      data-testid="plugin-composer-action"
      className={[
        "inline-flex h-8 items-center justify-center gap-1 rounded-md border border-surface-700 bg-surface-800 px-2.5 text-[12px]",
        toneClasses(entryTone(entry)),
        "hover:bg-surface-700 disabled:cursor-not-allowed disabled:opacity-60 transition-colors duration-100",
      ].join(" ")}
    >
      {posting ? (
        <Spinner className="size-3.5" />
      ) : (
        iconComp && createElement(iconComp, { className: "size-3.5 shrink-0", "aria-hidden": true })
      )}
      <span className="max-w-24 truncate">{label}</span>
    </button>
  );
}

/** A row's initials bubble, from an `avatar` string (a reviewer's initials, an
 *  org's short tag). Clipped to 3 characters so a plugin cannot stretch the row
 *  by pushing a long string through it. */
function RowAvatar({ initials }: { initials: string }) {
  return (
    <span
      className="flex size-5 shrink-0 items-center justify-center rounded-full bg-surface-700 font-mono text-[9px] text-text-secondary"
      aria-hidden
    >
      {initials.slice(0, 3)}
    </span>
  );
}

/** A detail row. Two lines at most: `prefix` + `label` lead the first with
 *  `value` pinned right, and `sublabel` leads the second with `badges` pinned
 *  right. Interactivity is layered: a `method` makes the row body a button that
 *  fires that worker method, and an `href` alongside it becomes a separate
 *  trailing open-externally affordance (so a selectable row can still link out).
 *  With `href` alone the whole row is the link, as before. */
function BlockRow({
  block,
  pluginId,
  sessionId,
}: {
  block: Record<string, unknown>;
  pluginId: string;
  sessionId?: string;
}) {
  const label = str(block, "label");
  const value = str(block, "value");
  const prefix = str(block, "prefix");
  const sublabel = str(block, "sublabel");
  const avatar = str(block, "avatar");
  const iconComp = lucideIcon(str(block, "icon"));
  const tone = validTone(block.tone);
  const valueTone = validTone(block.value_tone);
  // A validated hex `color` overrides the tone color for the icon, prefix and
  // value (e.g. a merged PR's purple, which no semantic tone names).
  const accent = accentStyle(block.color);
  const safe = safeHref(str(block, "href"));
  const method = str(block, "method");
  const selected = block.selected === true;
  const tooltip = str(block, "tooltip");
  // `mono` monospaces the row's own text (numbers, refs, durations). The prefix
  // and badges are always mono: they exist for exactly that kind of token.
  const mono = block.mono === true ? "font-mono" : "";
  const badges = objectList(block, "badges") ?? [];
  const { busy, run } = usePaneActionRunner(pluginId, sessionId);
  if (!label && !value && !prefix && !iconComp && !avatar) return null;
  // Name the link/button from its text so an icon-only row is not announced
  // unlabeled.
  const ariaLabel = [prefix, label, value, sublabel].filter(Boolean).join(" · ") || undefined;
  const secondLine = sublabel || badges.length > 0;
  const inner = (
    <span className="flex min-w-0 flex-col gap-0.5">
      <span className="flex min-w-0 items-center gap-2">
        {avatar ? (
          <RowAvatar initials={avatar} />
        ) : (
          iconComp &&
          createElement(iconComp, {
            className: `size-4 shrink-0 ${accent ? "" : toneTextClass(tone)}`,
            style: accent,
            "aria-hidden": true,
          })
        )}
        {prefix && (
          <span className={`shrink-0 font-mono ${accent ? "" : toneTextClass(tone)}`} style={accent}>
            {prefix}
          </span>
        )}
        {label && (
          <span className={`min-w-0 truncate ${mono} ${selected ? "text-text-bright" : "text-text-primary"}`}>
            {label}
          </span>
        )}
        {value && (
          // `value_tone` decouples the trailing token from the row's tone, for the
          // common shape of a status-colored glyph beside a neutral scalar (a
          // timestamp, a duration) that is not itself a status.
          <span
            className={`ml-auto shrink-0 font-mono text-[11px] ${accent && !valueTone ? "" : toneTextClass(valueTone ?? tone)}`}
            style={valueTone ? undefined : accent}
          >
            {value}
          </span>
        )}
      </span>
      {secondLine && (
        <span className="flex min-w-0 items-center gap-2">
          {sublabel && <span className={`min-w-0 truncate font-mono text-[10px] text-text-dim`}>{sublabel}</span>}
          {badges.length > 0 && (
            <span className="ml-auto flex shrink-0 items-center gap-1.5">
              {badges.map((b, i) => (
                <RowSignal key={i} badge={b} />
              ))}
            </span>
          )}
        </span>
      )}
    </span>
  );

  // A selected row reads as the pane's current subject, so it gets the brand
  // accent ring rather than a tone (tones are already spoken for by status).
  const shell = selected
    ? "rounded border border-brand-500/40 bg-brand-500/10"
    : method
      ? "rounded border border-surface-700/60"
      : "";

  if (method) {
    return (
      <div className={`flex items-stretch text-xs ${shell}`} data-testid="plugin-row-selectable">
        <button
          type="button"
          onClick={() => run(method, actionParams(block))}
          disabled={busy}
          aria-busy={busy || undefined}
          aria-pressed={selected}
          title={tooltip || undefined}
          aria-label={ariaLabel}
          className="flex min-w-0 flex-1 cursor-pointer items-center gap-2 px-1.5 py-1 text-left hover:bg-surface-700/40 disabled:cursor-default"
        >
          {busy && <Spinner className="size-3 shrink-0" />}
          {inner}
        </button>
        {safe && (
          <a
            className="flex w-7 shrink-0 items-center justify-center border-l border-surface-700/50 text-text-dim hover:bg-surface-700/40 hover:text-brand-500"
            href={safe}
            target="_blank"
            rel="noopener noreferrer"
            title="Open externally"
            aria-label={ariaLabel ? `Open ${ariaLabel} externally` : "Open externally"}
          >
            <ArrowUpRight className="size-3.5" aria-hidden />
          </a>
        )}
      </div>
    );
  }

  return safe ? (
    <a
      className="block rounded px-1 py-0.5 text-xs hover:bg-surface-700/40"
      href={safe}
      target="_blank"
      rel="noopener noreferrer"
      title={tooltip || undefined}
      aria-label={ariaLabel}
    >
      {inner}
    </a>
  ) : (
    <div className="px-1 py-0.5 text-xs" title={tooltip || undefined}>
      {inner}
    </div>
  );
}

/** One compact signal on a row's second line: a tone-tinted glyph or short
 *  token with a tooltip (CI state, review state, conflict state). Distinct from
 *  `BadgeChip` in that it carries no pill background, so a run of them reads as
 *  a status strip rather than competing with the row's own text. */
function RowSignal({ badge }: { badge: Record<string, unknown> }) {
  const text = str(badge, "text");
  const iconComp = lucideIcon(str(badge, "icon"));
  const tone = validTone(badge.tone);
  const accent = accentStyle(badge.color);
  const tooltip = str(badge, "tooltip");
  if (!text && !iconComp) return null;
  return (
    <span
      className={`inline-flex items-center gap-0.5 font-mono text-[10px] ${accent ? "" : toneTextClass(tone)}`}
      style={accent}
      title={tooltip || text || undefined}
      aria-label={text ? undefined : tooltip || undefined}
    >
      {iconComp && createElement(iconComp, { className: "size-3 shrink-0", "aria-hidden": true })}
      {text}
    </span>
  );
}

/** The repo's inline spinner glyph (same shape as the dialog buttons), sized to
 *  fit alongside slot text. `currentColor` so it inherits the surrounding tone. */
function Spinner({ className }: { className: string }) {
  return (
    <svg className={`animate-spin ${className}`} viewBox="0 0 24 24" aria-hidden>
      <circle className="opacity-25" cx="12" cy="12" r="10" stroke="currentColor" strokeWidth="4" fill="none" />
      <path className="opacity-75" fill="currentColor" d="M4 12a8 8 0 018-8V0C5.373 0 0 5.373 0 12h4z" />
    </svg>
  );
}

// A pane action's spinner clears after this even if no fresh state arrives, so
// a worker that processes the action without re-pushing (no sessions, ignored
// method, mid-refresh crash) can never leave the button spinning forever.
const ACTION_TIMEOUT_MS = 15000;

/** The click -> POST -> wait-for-fresh-state lifecycle every interactive pane
 *  element shares (`action` blocks, `callout` buttons, `method`-bearing rows).
 *  The worker runs the method and re-pushes its UI state, which a later poll
 *  renders; `busy` stays true from the click until the plugin's UI revision
 *  moves off the baseline the action POST returned (the worker's re-pushed state
 *  has landed), not merely until the POST is accepted, with a hard timeout
 *  fallback so it can never hang. A failed POST clears it at once. */
function usePaneActionRunner(pluginId: string, sessionId?: string) {
  const revision = usePluginUiRevision(pluginId, sessionId);
  const poke = usePluginUiPoke();
  // `posting`: the POST is in flight. `waitBaseline`: the revision the host had
  // when it accepted the action; the caller keeps spinning until the polled
  // revision moves off it or the timeout fires. Stored as state so a polled
  // revision change re-renders.
  const [posting, setPosting] = useState(false);
  const [waitBaseline, setWaitBaseline] = useState<number | null>(null);
  // Guards a same-tick double-fire of the POST, before `posting` commits and
  // `disabled` takes effect; the wait phase is guarded by `busy`/`disabled`.
  const postingRef = useRef(false);
  const timerRef = useRef<ReturnType<typeof setTimeout> | null>(null);

  // Derived in render, so nothing has to chase the revision in an effect: still
  // waiting only while the polled revision has not moved off the baseline. `!==`
  // so a daemon restart that resets the counter to a lower value also clears.
  const waiting = waitBaseline !== null && revision === waitBaseline;
  const busy = posting || waiting;

  // Only an unmount guard: stop the fallback timer so it cannot fire setState on
  // an unmounted block. No state writes here, so no effect-chases-event lint.
  useEffect(
    () => () => {
      if (timerRef.current) clearTimeout(timerRef.current);
    },
    [],
  );

  const run = async (method: string, params: Record<string, unknown> = {}) => {
    if (postingRef.current || busy) return;
    postingRef.current = true;
    setPosting(true);
    try {
      const accepted = await invokePluginAction(pluginId, method, sessionId, params);
      if (!accepted) return; // 403/404/network: nothing re-pushes, stop spinning
      poke();
      // No baseline (older daemon): can't track completion, so degrade to
      // clearing when the POST settles rather than spinning to the timeout.
      if (accepted.baselineRevision === null) return;
      // Hold the spinner until the revision moves off this baseline (see above).
      setWaitBaseline(accepted.baselineRevision);
      if (timerRef.current) clearTimeout(timerRef.current);
      timerRef.current = setTimeout(() => {
        setWaitBaseline(null);
        timerRef.current = null;
      }, ACTION_TIMEOUT_MS);
    } finally {
      postingRef.current = false;
      setPosting(false);
    }
  };

  return { busy, run };
}

/** An `action` pane block: a button that forwards a worker method (named by the
 *  plugin) to that plugin's worker, or, with `href` and no `method`, a link-out
 *  button for a state the host cannot perform itself. `disabled` renders it
 *  inert, which is how a blocked state reads (a merge gate, an unavailable
 *  operation) without pretending to be clickable. `variant: "primary"` gives the
 *  brand-filled treatment for a callout's main affordance. An icon is optional.
 *  `stretch` fills the width, which is how callout buttons lay out. */
function BlockAction({
  block,
  pluginId,
  sessionId,
  stretch = false,
}: {
  block: Record<string, unknown>;
  pluginId: string;
  sessionId?: string;
  stretch?: boolean;
}) {
  const label = str(block, "label");
  const method = str(block, "method");
  const iconComp = lucideIcon(str(block, "icon"));
  const disabled = block.disabled === true;
  const primary = str(block, "variant") === "primary";
  const tooltip = str(block, "tooltip");
  const safe = safeHref(str(block, "href"));
  const { busy, run } = usePaneActionRunner(pluginId, sessionId);

  // A label is the minimum; beyond that the button needs something to do, or an
  // explicit `disabled` saying it deliberately does nothing.
  if (!label || (!method && !safe && !disabled)) return null;

  const layout = stretch ? "w-full justify-center" : "self-start";
  const skin = primary
    ? "bg-brand-500 text-text-on-brand hover:bg-brand-400 font-semibold"
    : "bg-surface-700/50 text-text-secondary hover:text-text-primary hover:bg-surface-700";
  const className = `${layout} inline-flex items-center gap-1.5 rounded-md px-2 py-1.5 text-xs cursor-pointer ${skin} disabled:opacity-50 disabled:cursor-default transition-colors`;
  const leading = busy ? (
    <Spinner className="size-3.5" />
  ) : (
    iconComp && createElement(iconComp, { className: "size-3.5", "aria-hidden": true })
  );

  // A disabled action never navigates either: rendering the href as a live link
  // would let a blocked state be clicked through anyway.
  if (safe && !method && !disabled) {
    return (
      <a
        href={safe}
        target="_blank"
        rel="noopener noreferrer"
        title={tooltip || undefined}
        data-testid="plugin-pane-action"
        className={className}
      >
        {leading}
        {label}
        <ArrowUpRight className="size-3" aria-hidden />
      </a>
    );
  }

  return (
    <button
      type="button"
      onClick={method ? () => run(method, actionParams(block)) : undefined}
      disabled={busy || disabled || !method}
      aria-busy={busy || undefined}
      title={tooltip || undefined}
      data-testid="plugin-pane-action"
      className={className}
    >
      {leading}
      {label}
    </button>
  );
}

/** A `callout` pane block: the pane's headline verdict. A tone-bordered card
 *  carrying a glyph, a title, an optional detail paragraph, and its own action
 *  buttons laid out full width. This is the shape a "can I merge / is this
 *  blocked" summary wants, which a `section` (a titled list) cannot express. */
function BlockCallout({
  block,
  pluginId,
  sessionId,
}: {
  block: Record<string, unknown>;
  pluginId: string;
  sessionId?: string;
}) {
  const title = str(block, "title");
  const detail = str(block, "detail");
  const tone = validTone(block.tone);
  const accent = accentStyle(block.color);
  const iconComp = lucideIcon(str(block, "icon"));
  const actions = objectList(block, "actions") ?? [];
  if (!title && !detail) return null;
  const toneText = accent ? "" : toneTextClass(tone);
  return (
    <div
      className={`flex flex-col gap-2 rounded-md border p-2.5 ${calloutBorder(tone)} bg-surface-800/60`}
      data-testid="plugin-pane-callout"
    >
      <div className="flex items-start gap-2">
        {iconComp &&
          createElement(iconComp, {
            className: `mt-0.5 size-4 shrink-0 ${toneText}`,
            style: accent,
            "aria-hidden": true,
          })}
        <div className="flex min-w-0 flex-col gap-0.5">
          {title && (
            <div className={`text-xs font-semibold ${toneText}`} style={accent}>
              {title}
            </div>
          )}
          {detail && <div className="text-[11px] leading-snug text-text-secondary">{detail}</div>}
        </div>
      </div>
      {actions.map((a, i) => (
        <BlockAction key={i} block={a} pluginId={pluginId} sessionId={sessionId} stretch />
      ))}
    </div>
  );
}

/** Tone-matched border for a callout. Kept separate from `toneClasses` because
 *  that returns a fill + text pair for pills; a callout needs only the edge. */
function calloutBorder(tone: PluginUiTone | undefined): string {
  switch (tone) {
    case "info":
      return "border-status-unread/35";
    case "success":
      return "border-status-running/35";
    case "warn":
      return "border-status-waiting/35";
    case "danger":
      return "border-status-error/35";
    default:
      return "border-surface-700/60";
  }
}

/** A `bar` pane block: one proportional stacked bar over a list of weighted
 *  segments, for a ratio a number pair alone does not convey (added vs removed
 *  lines, passing vs failing counts). Segments with a non-positive or absent
 *  `value` are dropped; a bar left with nothing renders nothing. */
function BlockBar({ block }: { block: Record<string, unknown> }) {
  const segments = (objectList(block, "segments") ?? [])
    .map((s) => ({
      value: typeof s.value === "number" && Number.isFinite(s.value) ? s.value : 0,
      tone: validTone(s.tone),
      color: accentStyle(s.color),
      label: str(s, "label"),
    }))
    .filter((s) => s.value > 0);
  const caption = str(block, "caption");
  const total = segments.reduce((sum, s) => sum + s.value, 0);
  if (total <= 0) return null;
  return (
    <div className="flex flex-col gap-1" data-testid="plugin-pane-bar">
      <div className="flex h-1 gap-px overflow-hidden rounded-full">
        {segments.map((s, i) => (
          <span
            key={i}
            // The width is the whole point of the block, and it is a computed
            // percentage rather than a plugin string, so it cannot carry CSS.
            style={{ width: `${(s.value / total) * 100}%`, ...(s.color ? { backgroundColor: s.color.color } : {}) }}
            className={s.color ? "" : barFill(s.tone)}
            title={s.label || undefined}
          />
        ))}
      </div>
      {caption && <span className="text-[10px] text-text-dim">{caption}</span>}
    </div>
  );
}

/** Solid fill per tone for a bar segment (the pill classes are translucent,
 *  which reads as washed out at a 4px bar height). */
function barFill(tone: PluginUiTone | undefined): string {
  switch (tone) {
    case "info":
      return "bg-status-unread";
    case "success":
      return "bg-status-running";
    case "warn":
      return "bg-status-waiting";
    case "danger":
      return "bg-status-error";
    default:
      return "bg-status-idle";
  }
}

/** A read-only PR review comment: author, optional file:line, a wrapped body,
 *  and an unresolved/resolved marker. Wrapped in a link when `href` is a safe
 *  http(s) URL. A long body is clamped to 3 lines with a "more"/"less" toggle so
 *  the full text is reachable without leaving the pane. There are no
 *  reply/resolve controls; this only surfaces what is already on the PR. */
function BlockComment({ block }: { block: Record<string, unknown> }) {
  const author = str(block, "author");
  const body = str(block, "body");
  const path = str(block, "path");
  const line = typeof block.line === "number" ? block.line : undefined;
  const resolved = block.resolved === true;
  const safe = safeHref(str(block, "href"));
  const [expanded, setExpanded] = useState(false);
  const bodyId = useId();
  if (!author && !body) return null;
  const where = path ? `${path}${line ? `:${line}` : ""}` : undefined;
  // ponytail: cheap length/newline heuristic instead of measuring layout, so the
  // toggle works in jsdom and needs no ref/effect. Ceiling: a short-but-wide body
  // that wraps past 3 lines under 200 chars misses the toggle; raise the bound if
  // that bites.
  const longBody = !!body && (body.length > 200 || (body.match(/\n/g)?.length ?? 0) >= 3);
  // The linkable content (header + body); the toggle stays a sibling so it is
  // never an interactive child of the <a> (invalid nesting, odd keyboard focus).
  const linkContent = (
    <>
      <div className="flex items-center justify-between gap-2 text-text-secondary">
        <span className="min-w-0 truncate font-medium">{author}</span>
        <span className="flex shrink-0 items-center gap-1.5">
          {where && <span className="font-mono text-[10px] text-text-dim truncate max-w-40">{where}</span>}
          <span className={`text-[10px] ${resolved ? "text-status-running" : "text-status-waiting"}`}>
            {resolved ? "resolved" : "unresolved"}
          </span>
        </span>
      </div>
      {body && (
        // Clamp only when there is a toggle to undo it, so a short body that
        // still wraps past three lines is not truncated with no way to expand.
        <div
          id={bodyId}
          className={`mt-0.5 whitespace-pre-wrap text-text-primary ${longBody && !expanded ? "line-clamp-3" : ""}`}
        >
          {body}
        </div>
      )}
    </>
  );
  return (
    <div className="rounded-md bg-surface-700/30 p-2 text-xs">
      {safe ? (
        <a className="block rounded-md hover:bg-surface-700/50" href={safe} target="_blank" rel="noopener noreferrer">
          {linkContent}
        </a>
      ) : (
        linkContent
      )}
      {longBody && (
        <button
          type="button"
          data-testid="plugin-comment-toggle"
          aria-expanded={expanded}
          aria-controls={bodyId}
          onClick={() => setExpanded((v) => !v)}
          className="mt-0.5 text-[10px] text-text-dim hover:text-text-primary cursor-pointer"
        >
          {expanded ? "less" : "more"}
        </button>
      )}
    </div>
  );
}

/** Render one pane block. The block vocabulary is forward-compatible:
 *  an unknown `kind` (or a known kind missing its required field) renders
 *  nothing rather than throwing, so a newer plugin can push kinds an older host
 *  has never heard of. */
function DetailBlock({
  block,
  pluginId,
  sessionId,
}: {
  block: Record<string, unknown>;
  pluginId: string;
  sessionId?: string;
}) {
  switch (str(block, "kind")) {
    case "heading": {
      const text = str(block, "text");
      return text ? <div className="font-semibold text-sm text-text-primary">{text}</div> : null;
    }
    case "row":
      return <BlockRow block={block} pluginId={pluginId} sessionId={sessionId} />;
    case "comment":
      return <BlockComment block={block} />;
    case "note": {
      const text = str(block, "text");
      return text ? <p className={`text-xs ${toneTextClass(validTone(block.tone))}`}>{text}</p> : null;
    }
    case "divider":
      return <hr className="border-surface-700/60" />;
    case "action":
      return <BlockAction block={block} pluginId={pluginId} sessionId={sessionId} />;
    case "callout":
      return <BlockCallout block={block} pluginId={pluginId} sessionId={sessionId} />;
    case "bar":
      return <BlockBar block={block} />;
    case "columns":
      return <BlockColumns block={block} pluginId={pluginId} sessionId={sessionId} />;
    case "section":
      return <BlockSection block={block} pluginId={pluginId} sessionId={sessionId} />;
    default:
      // Unknown kind: ignored, not rendered, never throws.
      return null;
  }
}

/** A `columns` pane block: its children laid out side by side in equal
 *  fractions, so two compact summaries share a row instead of stacking. A single
 *  child spans the full width, which is what makes an elided sibling (an empty
 *  linked-issues card, say) collapse cleanly rather than leaving a gap. Nesting
 *  is one level in practice; children are dispatched through the normal
 *  vocabulary, so a column is usually a `section`. */
function BlockColumns({
  block,
  pluginId,
  sessionId,
}: {
  block: Record<string, unknown>;
  pluginId: string;
  sessionId?: string;
}) {
  const children = Array.isArray(block.children) ? block.children.filter(isObject) : [];
  if (children.length === 0) return null;
  return (
    <div
      className={`grid gap-1.5 ${children.length > 1 ? "grid-cols-2" : "grid-cols-1"}`}
      data-testid="plugin-pane-columns"
    >
      {children.map((c, i) => (
        <DetailBlock key={i} block={c} pluginId={pluginId} sessionId={sessionId} />
      ))}
    </div>
  );
}

/** A `section` pane block: a titled group of child blocks. The header carries an
 *  optional tone-tinted icon plus, pinned right, either a `value` summary string
 *  or a run of `badges` (count pills). `boxed` draws it as a bordered card and
 *  `scroll` caps the body height so a long list scrolls inside the section
 *  instead of pushing the rest of the pane away; `collapsible` folds it. */
function BlockSection({
  block,
  pluginId,
  sessionId,
}: {
  block: Record<string, unknown>;
  pluginId: string;
  sessionId?: string;
}) {
  const title = str(block, "title");
  const children = Array.isArray(block.children) ? block.children.filter(isObject) : [];
  const body = children.map((c, i) => <DetailBlock key={i} block={c} pluginId={pluginId} sessionId={sessionId} />);
  // An optional tone-tinted icon on the title gives an at-a-glance status
  // even when the section is folded (e.g. a green check vs a red x).
  const tone = validTone(block.tone);
  const iconComp = lucideIcon(str(block, "icon"));
  const value = str(block, "value");
  const valueTone = validTone(block.value_tone);
  const badges = objectList(block, "badges") ?? [];
  const boxed = block.boxed === true;
  const scroll = block.scroll === true;
  const titleColor = iconComp || tone ? toneTextClass(tone) : "text-text-dim";
  const titleClass = `text-[11px] font-semibold uppercase tracking-wide ${titleColor}`;
  const summary = (value || badges.length > 0) && (
    <span className="ml-auto flex shrink-0 items-center gap-1.5">
      {value && <span className={`font-mono text-[10px] normal-case ${toneTextClass(valueTone)}`}>{value}</span>}
      {badges.map((b, i) => (
        <BadgeChip
          key={i}
          text={str(b, "text") || undefined}
          icon={str(b, "icon") || undefined}
          tone={validTone(b.tone)}
          tooltip={str(b, "tooltip") || undefined}
          slot="pane"
          pluginId={pluginId}
        />
      ))}
    </span>
  );
  const titleInner = (
    <>
      {iconComp && createElement(iconComp, { className: "size-3 shrink-0", "aria-hidden": true })}
      {title}
      {summary}
    </>
  );
  // A capped body scrolls in place. The cap is a fixed class rather than a
  // plugin-supplied length: a worker must not be able to size host chrome.
  const bodyClass = `flex flex-col gap-1 ${scroll ? "max-h-64 overflow-y-auto" : ""}`;
  const shell = boxed ? "rounded-md border border-surface-700/60 bg-surface-800/40 p-2" : "";
  // A `collapsible` section folds via a native <details>: keyboard-accessible
  // and stateless, no JS toggle to track. `collapsed` sets the initial state;
  // it stays open by default so existing panes look unchanged.
  if (block.collapsible === true) {
    return (
      <details className={`group flex flex-col gap-1 ${shell}`} open={block.collapsed !== true}>
        <summary className={`flex cursor-pointer list-none items-center gap-1 select-none ${titleClass}`}>
          <ChevronRight className="size-3 shrink-0 transition-transform group-open:rotate-90" aria-hidden />
          {titleInner}
        </summary>
        <div className={bodyClass}>{body}</div>
      </details>
    );
  }
  return (
    <section className={`flex flex-col gap-1 ${shell}`}>
      {(title || summary) && <div className={`flex items-center gap-1 ${titleClass}`}>{titleInner}</div>}
      <div className={bodyClass}>{body}</div>
    </section>
  );
}

/** pane: the body of one dockable plugin pane. An entry is either a `blocks`
 *  list (the flexible, forward-compatible form) or the simple `{ title, body }`
 *  form. The dock supplies the frame (title bar, move, close) and the
 *  `default_location`; this renders only the scrollable content. */
export function PluginPaneBody({ entry }: { entry: PluginUiEntry }) {
  const blocks = objectList(entry.payload, "blocks");
  const title = payloadStr(entry, "title");
  const body = payloadStr(entry, "body");
  const footer = isObject(entry.payload.footer) ? entry.payload.footer : undefined;
  // A background poll only flips this once it outlasts the indicator delay, so
  // this surfaces a slow auto-refresh without strobing on every 3s cadence.
  const refreshing = usePluginUiRefreshing();
  return (
    <div className="flex flex-1 min-h-0 flex-col" data-testid="plugin-pane-body" data-plugin-id={entry.plugin_id}>
      <div className="min-h-0 flex-1 overflow-auto p-3">
        {refreshing && (
          <div
            className="sticky top-0 z-10 mb-1.5 flex items-center justify-end gap-1 text-[10px] text-text-dim"
            data-testid="plugin-pane-refreshing"
          >
            <Spinner className="size-3" />
            Refreshing…
          </div>
        )}
        {blocks ? (
          <div className="flex flex-col gap-1.5">
            {blocks.map((b, i) => (
              <DetailBlock key={i} block={b} pluginId={entry.plugin_id} sessionId={entry.session_id} />
            ))}
          </div>
        ) : (
          <>
            {title && <div className="font-semibold text-sm text-text-primary">{title}</div>}
            {body && <div className="mt-1 text-xs text-text-secondary whitespace-pre-wrap">{body}</div>}
          </>
        )}
      </div>
      {footer && <PluginPaneFooter footer={footer} />}
    </div>
  );
}

/** The pane's pinned status line. Sits outside the scroll area so it stays put
 *  while the blocks scroll: `text` on the left, tone-colored `value` on the
 *  right. A footer with neither renders nothing rather than an empty bar. */
function PluginPaneFooter({ footer }: { footer: Record<string, unknown> }) {
  const text = str(footer, "text");
  const value = str(footer, "value");
  const iconComp = lucideIcon(str(footer, "icon"));
  const tone = validTone(footer.tone);
  if (!text && !value) return null;
  return (
    <div
      className="flex shrink-0 items-center gap-1.5 border-t border-surface-700/60 px-3 py-1.5 font-mono text-[10px] text-text-dim"
      data-testid="plugin-pane-footer"
    >
      {iconComp && createElement(iconComp, { className: "size-3 shrink-0", "aria-hidden": true })}
      {text && <span className="min-w-0 truncate">{text}</span>}
      {value && <span className={`ml-auto shrink-0 ${toneTextClass(tone)}`}>{value}</span>}
    </div>
  );
}

/** The routed full-page slot (#2985). A plugin declaring `settings-page` gets a
 *  Settings nav entry (see SettingsView); this renders the global UI-state entry
 *  it pushed for that `(plugin_id, id)` through the same block vocabulary as a
 *  pane. The nav entry exists on declaration, so the entry may not be pushed yet
 *  (worker still starting, or nothing to show): render an explicit waiting state
 *  rather than a blank page. The entry is global (no `session_id`), so the reused
 *  PluginPaneBody dispatches its `action` blocks session-lessly. */
export function PluginSettingsPage({
  pluginId,
  contribId,
  pluginName,
}: {
  pluginId: string;
  contribId: string;
  pluginName: string;
}) {
  const entries = usePluginUiEntries();
  const entry = globalEntries(entries, "settings-page").find((e) => e.plugin_id === pluginId && e.id === contribId);
  if (!entry) {
    return (
      <div className="flex items-center gap-2 text-sm text-text-dim" data-testid="plugin-settings-page-waiting">
        <Spinner className="size-3.5" />
        Waiting for {pluginName} to load this page…
      </div>
    );
  }
  return <PluginSettingsPageBody entry={entry} />;
}

/** Separate component so the reused PluginPaneBody is only mounted once an entry
 *  exists; it renders in a full-width settings container, not the docked pane. */
function PluginSettingsPageBody({ entry }: { entry: PluginUiEntry }) {
  return (
    <div className="flex flex-col min-h-0" data-testid="plugin-settings-page" data-plugin-id={entry.plugin_id}>
      <PluginPaneBody entry={entry} />
    </div>
  );
}
