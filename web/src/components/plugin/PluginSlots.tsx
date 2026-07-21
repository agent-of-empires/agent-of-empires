// Renderers for the host-rendered plugin UI slots (#2366). The host ships
// typed display state; these components draw it. No plugin code runs here.
// Each reads the shared snapshot via context and the pure selectors in
// `pluginUi.ts`. Slots shipped here: status-bar, row-badge, row-column, card,
// pane, detail-badge, composer-action, settings-page. Notifications surface as toasts via the hook; the
// sort-key and filter-facet slots render as sidebar sort options and a facet
// filter (the sidebar owns those; see SidebarSortPicker / WorkspaceSidebar, #2401).

import { createElement, useEffect, useId, useRef, useState } from "react";
import { ChevronRight } from "lucide-react";

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

/** A clickable-when-href detail row: tone-tinted icon, primary label, secondary
 *  value, muted sublabel. */
function BlockRow({ block }: { block: Record<string, unknown> }) {
  const label = str(block, "label");
  const value = str(block, "value");
  const sublabel = str(block, "sublabel");
  const iconComp = lucideIcon(str(block, "icon"));
  const tone = validTone(block.tone);
  // A validated hex `color` overrides the tone color for the icon and value
  // (e.g. a merged PR's purple, which no semantic tone names).
  const accent = accentStyle(block.color);
  const safe = safeHref(str(block, "href"));
  if (!label && !value && !iconComp) return null;
  // Name the link from its text so an icon-only row is not announced unlabeled.
  const ariaLabel = [label, value, sublabel].filter(Boolean).join(" · ") || undefined;
  const inner = (
    <span className="flex min-w-0 items-center gap-2">
      {iconComp &&
        createElement(iconComp, {
          className: `size-4 shrink-0 ${accent ? "" : toneTextClass(tone)}`,
          style: accent,
          "aria-hidden": true,
        })}
      <span className="min-w-0 truncate">
        {label && <span className="font-medium text-text-primary">{label}</span>}
        {value && (
          <span className="ml-1.5 text-text-secondary" style={accent}>
            {value}
          </span>
        )}
        {sublabel && <span className="ml-1.5 text-[11px] text-text-dim">{sublabel}</span>}
      </span>
    </span>
  );
  return safe ? (
    <a
      className="block rounded px-1 py-0.5 text-xs hover:bg-surface-700/40"
      href={safe}
      target="_blank"
      rel="noopener noreferrer"
      aria-label={ariaLabel}
    >
      {inner}
    </a>
  ) : (
    <div className="px-1 py-0.5 text-xs">{inner}</div>
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

/** An `action` pane block: a button that forwards a worker method (named by the
 *  plugin) to that plugin's worker. The worker runs the method and re-pushes its
 *  UI state, which a later poll renders. The button spins from the click until
 *  the plugin's UI revision moves off the baseline the action POST returned (the
 *  worker's re-pushed state has landed), not merely until the POST is accepted,
 *  with a hard timeout fallback so it can never hang. A failed POST clears the
 *  spinner at once. An icon is optional. */
function BlockAction({
  block,
  pluginId,
  sessionId,
}: {
  block: Record<string, unknown>;
  pluginId: string;
  sessionId?: string;
}) {
  const label = str(block, "label");
  const method = str(block, "method");
  const iconComp = lucideIcon(str(block, "icon"));
  const revision = usePluginUiRevision(pluginId, sessionId);
  const poke = usePluginUiPoke();
  // `posting`: the POST is in flight. `waitBaseline`: the revision the host had
  // when it accepted the action; the button keeps spinning until the polled
  // revision moves off it (the worker's re-pushed state landed) or the timeout
  // fires. Stored as state so a polled revision change re-renders the button.
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

  if (!label || !method) return null;
  const onClick = async () => {
    if (postingRef.current || busy) return;
    postingRef.current = true;
    setPosting(true);
    try {
      const accepted = await invokePluginAction(pluginId, method, sessionId);
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
  return (
    <button
      type="button"
      onClick={onClick}
      disabled={busy}
      aria-busy={busy || undefined}
      data-testid="plugin-pane-action"
      className="self-start inline-flex items-center gap-1.5 rounded-md px-2 py-1 text-xs cursor-pointer bg-surface-700/50 text-text-secondary hover:text-text-primary hover:bg-surface-700 disabled:opacity-50 disabled:cursor-default transition-colors"
    >
      {busy ? (
        <Spinner className="size-3.5" />
      ) : (
        iconComp && createElement(iconComp, { className: "size-3.5", "aria-hidden": true })
      )}
      {label}
    </button>
  );
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
      return <BlockRow block={block} />;
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
    case "section": {
      const title = str(block, "title");
      const children = Array.isArray(block.children) ? block.children.filter(isObject) : [];
      const body = children.map((c, i) => <DetailBlock key={i} block={c} pluginId={pluginId} sessionId={sessionId} />);
      // An optional tone-tinted icon on the title gives an at-a-glance status
      // even when the section is folded (e.g. a green check vs a red x).
      const tone = validTone(block.tone);
      const iconComp = lucideIcon(str(block, "icon"));
      const titleColor = iconComp || tone ? toneTextClass(tone) : "text-text-dim";
      const titleClass = `text-[11px] font-semibold uppercase tracking-wide ${titleColor}`;
      const titleInner = (
        <>
          {iconComp && createElement(iconComp, { className: "size-3 shrink-0", "aria-hidden": true })}
          {title}
        </>
      );
      // A `collapsible` section folds via a native <details>: keyboard-accessible
      // and stateless, no JS toggle to track. `collapsed` sets the initial state;
      // it stays open by default so existing panes look unchanged.
      if (block.collapsible === true) {
        return (
          <details className="group flex flex-col gap-1" open={block.collapsed !== true}>
            <summary className={`flex cursor-pointer list-none items-center gap-1 select-none ${titleClass}`}>
              <ChevronRight className="size-3 shrink-0 transition-transform group-open:rotate-90" aria-hidden />
              {titleInner}
            </summary>
            <div className="flex flex-col gap-1">{body}</div>
          </details>
        );
      }
      return (
        <section className="flex flex-col gap-1">
          {title && <div className={`flex items-center gap-1 ${titleClass}`}>{titleInner}</div>}
          {body}
        </section>
      );
    }
    default:
      // Unknown kind: ignored, not rendered, never throws.
      return null;
  }
}

/** pane: the body of one dockable plugin pane. An entry is either a `blocks`
 *  list (the flexible, forward-compatible form) or the simple `{ title, body }`
 *  form. The dock supplies the frame (title bar, move, close) and the
 *  `default_location`; this renders only the scrollable content. */
export function PluginPaneBody({ entry }: { entry: PluginUiEntry }) {
  const blocks = objectList(entry.payload, "blocks");
  const title = payloadStr(entry, "title");
  const body = payloadStr(entry, "body");
  // A background poll only flips this once it outlasts the indicator delay, so
  // this surfaces a slow auto-refresh without strobing on every 3s cadence.
  const refreshing = usePluginUiRefreshing();
  return (
    <div className="flex-1 min-h-0 overflow-auto p-3" data-testid="plugin-pane-body" data-plugin-id={entry.plugin_id}>
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
