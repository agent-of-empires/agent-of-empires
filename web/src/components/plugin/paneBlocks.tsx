// Renderers for the plugin pane block vocabulary; a block missing required fields renders nothing.

import { useEffect, useId, useRef, useState, type ReactNode } from "react";
import { ArrowUpRight, ChevronRight } from "lucide-react";

import { invokePluginAction, type PluginUiTone } from "../../lib/api";
import { usePluginUiPoke, usePluginUiRevision } from "../../lib/pluginUiContext";
import { accentStyle, lucideIcon, toneTextClass, validTone } from "../../lib/pluginUi";
import { isInternalHref } from "../../lib/pluginHref";
import { BadgeChip, Spinner } from "./SlotChrome";
import { isObject, objectList, pluginLinkProps, renderIcon, safeHref, str, type Obj } from "./slotPayload";

interface BlockProps {
  block: Obj;
  pluginId: string;
  sessionId?: string;
  /** Set by `BlockColumns`: a narrow column breaks long row text instead of ellipsizing it. */
  wrap?: boolean;
}

// Forwarded verbatim with the action; the host injects the authoritative session_id.
function actionParams(block: Obj): Obj | undefined {
  return isObject(block.params) ? block.params : undefined;
}

function children(block: Obj): Obj[] {
  return Array.isArray(block.children) ? block.children.filter(isObject) : [];
}

/** `label`, suffixed with "externally" unless `href` stays inside aoe. */
function linkSuffix(href: string, label: string): string {
  return isInternalHref(href) ? label : `${label} externally`;
}

// Clears the spinner even if the worker never re-pushes state.
const ACTION_TIMEOUT_MS = 15000;

/** POST an action, then stay busy until the plugin's UI revision moves off the
 *  baseline the POST returned (or the timeout fires). A failed POST clears at once. */
function usePaneActionRunner(pluginId: string, sessionId?: string) {
  const revision = usePluginUiRevision(pluginId, sessionId);
  const poke = usePluginUiPoke();
  const [posting, setPosting] = useState(false);
  const [waitBaseline, setWaitBaseline] = useState<number | null>(null);
  // Guards a same-tick double click before `posting` commits.
  const postingRef = useRef(false);
  const timerRef = useRef<ReturnType<typeof setTimeout> | null>(null);
  // `===` rather than `>`: a daemon restart may reset the counter lower.
  const busy = posting || (waitBaseline !== null && revision === waitBaseline);

  useEffect(
    () => () => {
      if (timerRef.current) clearTimeout(timerRef.current);
    },
    [],
  );

  const run = async (method: string, params: Obj = {}) => {
    if (postingRef.current || busy) return;
    postingRef.current = true;
    setPosting(true);
    try {
      const accepted = await invokePluginAction(pluginId, method, sessionId, params);
      if (!accepted) return;
      poke();
      // Older daemons return no baseline; clear when the POST settles.
      if (accepted.baselineRevision === null) return;
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

/** A compact tone-tinted glyph or token for a row's second line, without a pill. */
function RowSignal({ badge, wrap }: { badge: Obj; wrap?: boolean }) {
  const text = str(badge, "text");
  const icon = lucideIcon(str(badge, "icon"));
  const accent = accentStyle(badge.color);
  const tooltip = str(badge, "tooltip");
  if (!text && !icon) return null;
  return (
    <span
      className={`inline-flex items-center gap-0.5 font-mono text-[10px] ${wrap ? "min-w-0 wrap-anywhere" : ""} ${accent ? "" : toneTextClass(validTone(badge.tone))}`}
      style={accent}
      title={tooltip || text || undefined}
      aria-label={text ? undefined : tooltip || undefined}
    >
      {renderIcon(icon, "size-3 shrink-0")}
      {text}
    </span>
  );
}

/** Up to two lines. A `method` makes the body a button, with any `href` as a
 *  separate trailing link; with `href` alone the whole row is the link. */
function BlockRow({ block, pluginId, sessionId, wrap }: BlockProps) {
  const label = str(block, "label");
  const value = str(block, "value");
  const prefix = str(block, "prefix");
  const sublabel = str(block, "sublabel");
  const avatar = str(block, "avatar");
  const icon = lucideIcon(str(block, "icon"));
  const tone = validTone(block.tone);
  const valueTone = validTone(block.value_tone);
  const accent = accentStyle(block.color);
  const safe = safeHref(str(block, "href"));
  const method = str(block, "method");
  const selected = block.selected === true;
  const tooltip = str(block, "tooltip") || undefined;
  const mono = block.mono === true ? "font-mono" : "";
  const badges = objectList(block, "badges") ?? [];
  const { busy, run } = usePaneActionRunner(pluginId, sessionId);
  if (!label && !value && !prefix && !icon && !avatar) return null;
  const ariaLabel = [prefix, label, value, sublabel].filter(Boolean).join(" · ") || undefined;
  const toneText = accent ? "" : toneTextClass(tone);
  // wrap-anywhere (not break-words): break-word doesn't shrink a flex item's
  // automatic min size, so an unbroken token (a session id, a branch name)
  // would still blow out the column.
  const textClamp = wrap ? "wrap-anywhere" : "truncate";
  const align = wrap ? "items-start" : "items-center";
  const inner = (
    <span className="flex min-w-0 flex-col gap-0.5">
      <span className={`flex min-w-0 ${align} gap-2`}>
        {avatar ? (
          <span
            className="flex size-5 shrink-0 items-center justify-center rounded-full bg-surface-700 font-mono text-[9px] text-text-secondary"
            aria-hidden
          >
            {avatar.slice(0, 3)}
          </span>
        ) : (
          renderIcon(icon, `size-4 shrink-0 ${toneText}`, accent)
        )}
        {prefix && (
          <span className={`${wrap ? "min-w-0 wrap-anywhere" : "shrink-0"} font-mono ${toneText}`} style={accent}>
            {prefix}
          </span>
        )}
        {label && (
          <span className={`min-w-0 ${textClamp} ${mono} ${selected ? "text-text-bright" : "text-text-primary"}`}>
            {label}
          </span>
        )}
        {value && (
          <span
            className={`ml-auto ${wrap ? "min-w-0 wrap-anywhere" : "shrink-0"} font-mono text-[11px] ${accent && !valueTone ? "" : toneTextClass(valueTone ?? tone)}`}
            style={valueTone ? undefined : accent}
          >
            {value}
          </span>
        )}
      </span>
      {(sublabel || badges.length > 0) && (
        <span className={`flex min-w-0 ${align} gap-2`}>
          {sublabel && <span className={`min-w-0 ${textClamp} font-mono text-[10px] text-text-dim`}>{sublabel}</span>}
          {badges.length > 0 && (
            <span className={`ml-auto flex items-center gap-1.5 ${wrap ? "min-w-0 shrink flex-wrap" : "shrink-0"}`}>
              {badges.map((b, i) => (
                <RowSignal key={i} badge={b} wrap={wrap} />
              ))}
            </span>
          )}
        </span>
      )}
    </span>
  );

  if (method) {
    const shell = selected
      ? "rounded border border-brand-500/40 bg-brand-500/10"
      : "rounded border border-surface-700/60";
    return (
      <div className={`flex items-stretch text-xs ${shell}`} data-testid="plugin-row-selectable">
        <button
          type="button"
          onClick={() => run(method, actionParams(block))}
          disabled={busy}
          aria-busy={busy || undefined}
          aria-pressed={selected}
          title={tooltip}
          aria-label={ariaLabel}
          className="flex min-w-0 flex-1 cursor-pointer items-center gap-2 px-1.5 py-1 text-left hover:bg-surface-700/40 disabled:cursor-default"
        >
          {busy && <Spinner className="size-3 shrink-0" />}
          {inner}
        </button>
        {safe && (
          <a
            className="flex w-7 shrink-0 items-center justify-center border-l border-surface-700/50 text-text-dim hover:bg-surface-700/40 hover:text-brand-500"
            {...pluginLinkProps(safe)}
            title={linkSuffix(safe, "Open")}
            aria-label={linkSuffix(safe, ariaLabel ? `Open ${ariaLabel}` : "Open")}
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
      {...pluginLinkProps(safe)}
      title={tooltip}
      aria-label={ariaLabel}
    >
      {inner}
    </a>
  ) : (
    <div className="px-1 py-0.5 text-xs" title={tooltip}>
      {inner}
    </div>
  );
}

/** A worker-method button, or a link-out when it has `href` but no `method`. */
function BlockAction({ block, pluginId, sessionId, stretch = false }: BlockProps & { stretch?: boolean }) {
  const label = str(block, "label");
  const method = str(block, "method");
  const icon = lucideIcon(str(block, "icon"));
  const disabled = block.disabled === true;
  const tooltip = str(block, "tooltip") || undefined;
  const safe = safeHref(str(block, "href"));
  const { busy, run } = usePaneActionRunner(pluginId, sessionId);
  if (!label || (!method && !safe && !disabled)) return null;

  const layout = stretch ? "w-full justify-center" : "self-start";
  const skin =
    str(block, "variant") === "primary"
      ? "bg-brand-500 text-text-on-brand hover:bg-brand-400 font-semibold"
      : "bg-surface-700/50 text-text-secondary hover:text-text-primary hover:bg-surface-700";
  const className = `${layout} inline-flex items-center gap-1.5 rounded-md px-2 py-1.5 text-xs cursor-pointer ${skin} disabled:opacity-50 disabled:cursor-default transition-colors`;
  const leading = busy ? <Spinner className="size-3.5" /> : renderIcon(icon, "size-3.5");

  // A disabled action must not stay clickable through its href.
  if (safe && !method && !disabled) {
    return (
      <a {...pluginLinkProps(safe)} title={tooltip} data-testid="plugin-pane-action" className={className}>
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
      title={tooltip}
      data-testid="plugin-pane-action"
      className={className}
    >
      {leading}
      {label}
    </button>
  );
}

const TONE_BORDER: Partial<Record<PluginUiTone, string>> = {
  info: "border-status-unread/35",
  success: "border-status-running/35",
  warn: "border-status-waiting/35",
  danger: "border-status-error/35",
};

// Solid fills: the translucent pill classes wash out at bar height.
const TONE_FILL: Partial<Record<PluginUiTone, string>> = {
  info: "bg-status-unread",
  success: "bg-status-running",
  warn: "bg-status-waiting",
  danger: "bg-status-error",
};

function BlockCallout({ block, pluginId, sessionId }: BlockProps) {
  const title = str(block, "title");
  const detail = str(block, "detail");
  const tone = validTone(block.tone);
  const accent = accentStyle(block.color);
  const actions = objectList(block, "actions") ?? [];
  if (!title && !detail) return null;
  const toneText = accent ? "" : toneTextClass(tone);
  return (
    <div
      className={`flex flex-col gap-2 rounded-md border p-2.5 ${(tone && TONE_BORDER[tone]) ?? "border-surface-700/60"} bg-surface-800/60`}
      data-testid="plugin-pane-callout"
    >
      <div className="flex items-start gap-2">
        {renderIcon(lucideIcon(str(block, "icon")), `mt-0.5 size-4 shrink-0 ${toneText}`, accent)}
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

function finite(v: unknown): v is number {
  return typeof v === "number" && Number.isFinite(v);
}

/** Each segment is colored by the highest `bands[].at` its right-hand value reaches. */
function BlockSparkline({ block }: { block: Obj }) {
  const values = Array.isArray(block.values) ? block.values.filter(finite) : [];
  if (values.length === 0) return null;
  const max = typeof block.max === "number" && block.max > 0 ? block.max : Math.max(...values, 0, Number.MIN_VALUE);
  const bands = (objectList(block, "bands") ?? [])
    .map((b) => ({ at: b.at, tone: validTone(b.tone) }))
    .filter((b): b is { at: number; tone: PluginUiTone } => finite(b.at) && b.tone !== undefined)
    .sort((a, b) => b.at - a.at);
  const baseTone = validTone(block.tone);
  const toneClass = (v: number) => toneTextClass(bands.find((b) => v >= b.at)?.tone ?? baseTone);

  const w = 120;
  const h = 24;
  const step = values.length > 1 ? w / (values.length - 1) : w;
  const y = (v: number) => h - Math.min(Math.max(v / max, 0), 1) * h;
  const caption = str(block, "caption");
  return (
    <div className="flex flex-col gap-1" data-testid="plugin-pane-sparkline">
      <svg viewBox={`0 0 ${w} ${h}`} width={w} height={h} preserveAspectRatio="none" role="img">
        {values.length === 1 ? (
          <circle cx={w / 2} cy={y(values[0]!)} r={1.5} className={toneClass(values[0]!)} fill="currentColor" />
        ) : (
          values
            .slice(1)
            .map((to, i) => (
              <line
                key={i}
                x1={i * step}
                y1={y(values[i]!)}
                x2={(i + 1) * step}
                y2={y(to)}
                className={toneClass(to)}
                stroke="currentColor"
                strokeWidth={1.5}
                vectorEffect="non-scaling-stroke"
              />
            ))
        )}
      </svg>
      {caption && <span className="text-[10px] text-text-dim">{caption}</span>}
    </div>
  );
}

/** A proportional stacked bar; non-positive segments are dropped. */
function BlockBar({ block }: { block: Obj }) {
  const segments = (objectList(block, "segments") ?? [])
    .map((s) => ({
      value: finite(s.value) ? s.value : 0,
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
            style={{ width: `${(s.value / total) * 100}%`, ...(s.color ? { backgroundColor: s.color.color } : {}) }}
            className={s.color ? "" : ((s.tone && TONE_FILL[s.tone]) ?? "bg-status-idle")}
            title={s.label || undefined}
          />
        ))}
      </div>
      {caption && <span className="text-[10px] text-text-dim">{caption}</span>}
    </div>
  );
}

/** A read-only review comment; long bodies clamp to 3 lines behind a more/less toggle. */
function BlockComment({ block }: { block: Obj }) {
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
  // Length heuristic instead of layout measurement, so it works without refs or jsdom layout.
  const longBody = !!body && (body.length > 200 || (body.match(/\n/g)?.length ?? 0) >= 3);
  // The toggle stays outside the link to avoid nesting interactive elements.
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
        <a className="block rounded-md hover:bg-surface-700/50" {...pluginLinkProps(safe)}>
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

function BlockColumns({ block, pluginId, sessionId, wrap }: BlockProps) {
  const kids = children(block);
  if (kids.length === 0) return null;
  // A lone child spans full width here, but it may still sit inside an
  // outer column narrow enough to need the inherited wrap.
  const childWrap = wrap || kids.length > 1;
  return (
    <div
      className={`grid gap-1.5 ${kids.length > 1 ? "grid-cols-2" : "grid-cols-1"}`}
      data-testid="plugin-pane-columns"
    >
      {kids.map((c, i) => (
        <DetailBlock key={i} block={c} pluginId={pluginId} sessionId={sessionId} wrap={childWrap} />
      ))}
    </div>
  );
}

/** A titled group; `boxed` draws a card, `scroll` caps height, `collapsible` folds it. */
function BlockSection({ block, pluginId, sessionId, wrap }: BlockProps) {
  const title = str(block, "title");
  const body = children(block).map((c, i) => (
    <DetailBlock key={i} block={c} pluginId={pluginId} sessionId={sessionId} wrap={wrap} />
  ));
  const tone = validTone(block.tone);
  const icon = lucideIcon(str(block, "icon"));
  const value = str(block, "value");
  const badges = objectList(block, "badges") ?? [];
  const titleClass = `text-[11px] font-semibold uppercase tracking-wide ${icon || tone ? toneTextClass(tone) : "text-text-dim"}`;
  const summary = (value || badges.length > 0) && (
    <span className={`ml-auto flex items-center gap-1.5 ${wrap ? "min-w-0 shrink flex-wrap" : "shrink-0"}`}>
      {value && (
        <span
          className={`font-mono text-[10px] normal-case ${toneTextClass(validTone(block.value_tone))} ${wrap ? "wrap-anywhere" : ""}`}
        >
          {value}
        </span>
      )}
      {badges.map((b, i) => (
        <BadgeChip key={i} item={{ ...b, href: undefined }} slot="pane" pluginId={pluginId} />
      ))}
    </span>
  );
  const titleInner = (
    <>
      {renderIcon(icon, "size-3 shrink-0")}
      {title}
      {summary}
    </>
  );
  // Fixed cap class: a plugin must not size host chrome.
  const bodyClass = `flex flex-col gap-1 ${block.scroll === true ? "max-h-64 overflow-y-auto" : ""}`;
  const shell = block.boxed === true ? "rounded-md border border-surface-700/60 bg-surface-800/40 p-2" : "";
  if (block.collapsible === true) {
    // Uncontrolled <details> so a re-push does not undo the user's fold.
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

const KINDS: Record<string, (p: BlockProps) => ReactNode> = {
  heading: ({ block }) => {
    const text = str(block, "text");
    return text ? <div className="font-semibold text-sm text-text-primary">{text}</div> : null;
  },
  note: ({ block }) => {
    const text = str(block, "text");
    return text ? <p className={`text-xs ${toneTextClass(validTone(block.tone))}`}>{text}</p> : null;
  },
  divider: () => <hr className="border-surface-700/60" />,
  row: (p) => <BlockRow {...p} />,
  comment: ({ block }) => <BlockComment block={block} />,
  action: (p) => <BlockAction {...p} />,
  callout: (p) => <BlockCallout {...p} />,
  bar: ({ block }) => <BlockBar block={block} />,
  sparkline: ({ block }) => <BlockSparkline block={block} />,
  columns: (p) => <BlockColumns {...p} />,
  section: (p) => <BlockSection {...p} />,
};

/** Unknown kinds render nothing, so newer plugins degrade on older hosts. */
export function DetailBlock(props: BlockProps) {
  return KINDS[str(props.block, "kind") ?? ""]?.(props) ?? null;
}
