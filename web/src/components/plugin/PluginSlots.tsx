// Renderers for the host-rendered plugin UI slots (#2366). The host ships
// typed display state; these components draw it. No plugin code runs here.
// Each reads the shared snapshot via context and the pure selectors in
// `pluginUi.ts`. Slots shipped here: status-bar, row-badge, row-column, card,
// pane, detail-badge. Notifications surface as toasts via the hook;
// sort-key and filter-facet are deferred (see #2366 follow-ups).

import { createElement, useState } from "react";

import { invokePluginAction } from "../../lib/api";
import { usePluginUiEntries } from "../../lib/pluginUiContext";
import {
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

/** row-column: per-session text column, right-aligned on a session row. The
 *  payload may also carry sort/filter scalars; rendering those as interactive
 *  controls is the deferred sort-key/filter-facet work. */
export function PluginRowColumn({ sessionId }: { sessionId: string }) {
  const entries = sessionEntries(usePluginUiEntries(), "row-column", sessionId);
  if (entries.length === 0) return null;
  return (
    <span className="flex min-w-0 items-center gap-1.5">
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

/** A clickable-when-href detail row: tone-tinted icon, primary label, secondary
 *  value, muted sublabel. */
function BlockRow({ block }: { block: Record<string, unknown> }) {
  const label = str(block, "label");
  const value = str(block, "value");
  const sublabel = str(block, "sublabel");
  const iconComp = lucideIcon(str(block, "icon"));
  const tone = validTone(block.tone);
  const safe = safeHref(str(block, "href"));
  if (!label && !value && !iconComp) return null;
  // Name the link from its text so an icon-only row is not announced unlabeled.
  const ariaLabel = [label, value, sublabel].filter(Boolean).join(" · ") || undefined;
  const inner = (
    <span className="flex min-w-0 items-center gap-2">
      {iconComp &&
        createElement(iconComp, { className: `size-4 shrink-0 ${toneTextClass(tone)}`, "aria-hidden": true })}
      <span className="min-w-0 truncate">
        {label && <span className="font-medium text-text-primary">{label}</span>}
        {value && <span className="ml-1.5 text-text-secondary">{value}</span>}
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

/** An `action` pane block: a button that forwards a worker method (named by the
 *  plugin) to that plugin's worker. Fire-and-forget; the worker re-pushes its
 *  UI state, which the next poll renders. Disabled briefly so a double-click
 *  does not double-fire. An icon is optional. */
function BlockAction({ block, pluginId }: { block: Record<string, unknown>; pluginId: string }) {
  const label = str(block, "label");
  const method = str(block, "method");
  const iconComp = lucideIcon(str(block, "icon"));
  const [busy, setBusy] = useState(false);
  if (!label || !method) return null;
  const onClick = async () => {
    if (busy) return;
    setBusy(true);
    try {
      await invokePluginAction(pluginId, method);
    } finally {
      setBusy(false);
    }
  };
  return (
    <button
      type="button"
      onClick={onClick}
      disabled={busy}
      data-testid="plugin-pane-action"
      className="self-start inline-flex items-center gap-1.5 rounded-md px-2 py-1 text-xs cursor-pointer bg-surface-700/50 text-text-secondary hover:text-text-primary hover:bg-surface-700 disabled:opacity-50 disabled:cursor-default transition-colors"
    >
      {iconComp && createElement(iconComp, { className: "size-3.5", "aria-hidden": true })}
      {label}
    </button>
  );
}

/** Render one pane block. The block vocabulary is forward-compatible:
 *  an unknown `kind` (or a known kind missing its required field) renders
 *  nothing rather than throwing, so a newer plugin can push kinds an older host
 *  has never heard of. */
function DetailBlock({ block, pluginId }: { block: Record<string, unknown>; pluginId: string }) {
  switch (str(block, "kind")) {
    case "heading": {
      const text = str(block, "text");
      return text ? <div className="font-semibold text-sm text-text-primary">{text}</div> : null;
    }
    case "row":
      return <BlockRow block={block} />;
    case "note": {
      const text = str(block, "text");
      return text ? <p className={`text-xs ${toneTextClass(validTone(block.tone))}`}>{text}</p> : null;
    }
    case "divider":
      return <hr className="border-surface-700/60" />;
    case "action":
      return <BlockAction block={block} pluginId={pluginId} />;
    case "section": {
      const title = str(block, "title");
      const children = Array.isArray(block.children) ? block.children.filter(isObject) : [];
      return (
        <section className="flex flex-col gap-1">
          {title && <div className="text-[11px] font-semibold uppercase tracking-wide text-text-dim">{title}</div>}
          {children.map((c, i) => (
            <DetailBlock key={i} block={c} pluginId={pluginId} />
          ))}
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
  return (
    <div className="flex-1 min-h-0 overflow-auto p-3" data-testid="plugin-pane-body" data-plugin-id={entry.plugin_id}>
      {blocks ? (
        <div className="flex flex-col gap-1.5">
          {blocks.map((b, i) => (
            <DetailBlock key={i} block={b} pluginId={entry.plugin_id} />
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
