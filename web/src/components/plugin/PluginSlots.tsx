// Renderers for the host-rendered plugin UI slots (#2366). The host ships
// typed display state; these components draw it. No plugin code runs here.
// Each reads the shared snapshot via context and the pure selectors in
// `pluginUi.ts`. Slots shipped here: status-bar, row-badge, row-column, card,
// detail-panel, detail-badge. Notifications surface as toasts via the hook;
// sort-key and filter-facet are deferred (see #2366 follow-ups).

import { createElement } from "react";
import {
  CircleAlert,
  CircleCheck,
  CircleDot,
  Clock,
  GitMerge,
  GitPullRequestArrow,
  GitPullRequestClosed,
  GitPullRequestDraft,
  type LucideIcon,
} from "lucide-react";

import { usePluginUiEntries } from "../../lib/pluginUiContext";
import {
  entryText,
  entryTone,
  globalEntries,
  payloadStr,
  sessionEntries,
  toneClasses,
  toneTextClass,
  validTone,
} from "../../lib/pluginUi";
import type { PluginUiEntry, PluginUiTone } from "../../lib/api";

// Plugins name an icon by its lucide kebab name. The set is an explicit
// allowlist, not the whole lucide barrel: that keeps the bundle small and means
// a plugin can never name an arbitrary import. An unknown name renders nothing.
const ICONS: Record<string, LucideIcon> = {
  "git-pull-request-arrow": GitPullRequestArrow,
  "git-pull-request-draft": GitPullRequestDraft,
  "git-pull-request-closed": GitPullRequestClosed,
  "git-merge": GitMerge,
  "circle-alert": CircleAlert,
  "circle-check": CircleCheck,
  "circle-dot": CircleDot,
  clock: Clock,
};

function lucideIcon(name: string | undefined): LucideIcon | undefined {
  return name ? ICONS[name] : undefined;
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
  const className = `inline-flex max-w-48 min-w-0 items-center gap-1 truncate font-mono text-[11px] px-1.5 py-0.5 rounded-full ${toneClasses(tone)}`;
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

/** Render one detail-panel block. The block vocabulary is forward-compatible:
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

/** detail-panel: per-session panels in the session detail view. An entry is
 *  either a `blocks` list (the flexible, forward-compatible pane) or the simple
 *  `{ title, body }` form. */
export function PluginDetailPanels({ sessionId }: { sessionId: string }) {
  const entries = sessionEntries(usePluginUiEntries(), "detail-panel", sessionId);
  if (entries.length === 0) return null;
  return (
    <div className="flex flex-col gap-2" data-testid="plugin-detail-panels">
      {entries.map((e) => {
        const blocks = objectList(e.payload, "blocks");
        const title = payloadStr(e, "title");
        const body = payloadStr(e, "body");
        return (
          <section
            key={`${e.plugin_id}:${e.id}`}
            className="rounded-lg p-3 ring-1 ring-surface-700/60 bg-surface-800/40"
            data-plugin-id={e.plugin_id}
          >
            {blocks ? (
              <div className="flex flex-col gap-1.5">
                {blocks.map((b, i) => (
                  <DetailBlock key={i} block={b} pluginId={e.plugin_id} />
                ))}
              </div>
            ) : (
              <>
                {title && <div className="font-semibold text-sm text-text-primary">{title}</div>}
                {body && <div className="mt-1 text-xs text-text-secondary whitespace-pre-wrap">{body}</div>}
              </>
            )}
          </section>
        );
      })}
    </div>
  );
}
