/* eslint-disable react-refresh/only-export-components */
// Shared header, body blocks, and helpers for the per-kind tool cards.

import { useCallback, useEffect, useMemo, useState, type CSSProperties, type SetStateAction } from "react";
import { ArrowUpRight, ChevronDown, Copy as CopyIcon } from "lucide-react";

import { useShikiTheme } from "../../hooks/useShikiTheme";
import { parseJsonObject } from "../../lib/acpArgs";
import { useAcpPrefs } from "../../lib/acpPrefs";
import type { ActivityRow, ToolCall } from "../../lib/acpTypes";
import { hasAnsi, parseAnsi, type AnsiStyle } from "../../lib/ansi";
import { highlightSnippet } from "../../lib/snippetHighlighter";
import { useAcpFileRef } from "./AcpFileRefContext";
import { useToolDisplayMode, type ToolDensity } from "./ToolDisplayMode";

export interface ToolCardProps {
  tool: ToolCall;
  result?: ActivityRow;
}

export type Status = "running" | "ok" | "err" | "stopped";

export function statusFor(result?: ActivityRow): Status {
  if (!result) return "running";
  if (result.kind === "tool_error") return "err";
  if (result.kind === "tool_stopped") return "stopped";
  return "ok";
}

/** Keys AcpRuntime smuggles through `args_preview`; never shown as user input. */
export function isAcpBookkeepingKey(key: string): boolean {
  return (
    key === "_aoe_title" ||
    key === "_aoe_started_at" ||
    key === "_aoe_parent_tool_call_id" ||
    key === "_aoe_raw_tool_name"
  );
}

export function useToolArgs(tool: ToolCall) {
  return useMemo(() => parseJsonObject(tool.args_preview), [tool.args_preview]);
}

/** Pretty-printed args minus bookkeeping keys and `omit`; the raw preview when unparseable. */
export function useInputJson(tool: ToolCall, args: Record<string, unknown> | null, omit?: string) {
  return useMemo<string>(() => {
    if (!args) return tool.args_preview;
    const rest = Object.fromEntries(Object.entries(args).filter(([k]) => !isAcpBookkeepingKey(k) && k !== omit));
    return JSON.stringify(rest, null, 2);
  }, [args, tool.args_preview, omit]);
}

/** Earliest start to latest completion across a run of calls; open-ended while any runs. */
export function spanTimes(items: { tool: ToolCall; result?: ActivityRow }[]) {
  const startedAt = items
    .map((i) => i.tool.started_at)
    .sort()
    .at(0);
  const endedAt = items.every((i) => i.result)
    ? items
        .map((i) => i.result!.at)
        .sort()
        .at(-1)
    : undefined;
  return { startedAt, endedAt };
}

/** Expand state. Failed cards open by default and compact density closes the
 *  rest; a user toggle overrides the baseline only for the density it was made in. */
export function useToolCardExpansion(status: Status, defaultOpen = false) {
  const density = useToolDisplayMode();
  const baseline = status === "err" ? true : density === "compact" ? false : defaultOpen;
  const [override, setOverride] = useState<{ density: ToolDensity; open: boolean } | null>(null);
  const active = override && override.density === density ? override.open : null;
  const open = active ?? baseline;
  const setOpen = useCallback(
    (action: SetStateAction<boolean>) => {
      setOverride((prev) => {
        const current = prev && prev.density === density ? prev.open : baseline;
        const next = typeof action === "function" ? action(current) : action;
        return { density, open: next };
      });
    },
    [density, baseline],
  );
  return [open, setOpen] as const;
}

function StatusDot({ status, neutral }: { status: Status; neutral?: boolean }) {
  const cls =
    status === "running"
      ? "bg-brand-400 animate-pulse"
      : neutral || status === "stopped"
        ? "bg-text-dim/60"
        : status === "ok"
          ? "bg-status-running"
          : "bg-status-error";
  return <span className={`h-2 w-2 shrink-0 rounded-full ${cls}`} />;
}

function StatusBadge({ status }: { status: Status }) {
  if (status === "running") {
    return (
      <span className="inline-flex items-center gap-1 text-[11px] text-text-dim">
        <span className="h-1.5 w-1.5 rounded-full bg-brand-400 animate-pulse" />
        running
      </span>
    );
  }
  if (status === "err") {
    return <span className="text-[11px] text-status-error">failed</span>;
  }
  return <span className="text-[11px] text-text-dim">{status === "stopped" ? "stopped" : "done"}</span>;
}

interface CardChromeProps {
  status: Status;
  icon: React.ReactNode;
  label: string;
  primary: React.ReactNode;
  meta?: React.ReactNode;
  expanded: boolean;
  onToggle?: () => void;
  body?: React.ReactNode;
  /** Always rendered below the header, regardless of `expanded`. */
  subBody?: React.ReactNode;
  /** Once settled, show a neutral dot and no badge: a child failure does not fail the group. */
  neutralOnDone?: boolean;
  /** Header click navigates elsewhere, so show an arrow instead of a chevron. */
  navigate?: boolean;
  /** Durations count from the adapter's tool_call frame, which can precede the
   *  real subprocess start, so they run long. */
  startedAt?: string;
  endedAt?: string;
}

export function CardChrome({
  status,
  icon,
  label,
  primary,
  meta,
  expanded,
  onToggle,
  body,
  subBody,
  startedAt,
  endedAt,
  neutralOnDone,
  navigate,
}: CardChromeProps) {
  const { showToolDurations } = useAcpPrefs();
  const Header = onToggle ? "button" : "div";
  const showNeutral = neutralOnDone === true && status !== "running";
  return (
    <div className="my-1 overflow-hidden rounded-md border border-surface-700 bg-surface-800/50 text-sm">
      <Header
        type={onToggle ? "button" : undefined}
        onClick={onToggle}
        className={[
          "flex w-full items-center gap-2 px-3 py-1.5 text-left",
          onToggle ? "cursor-pointer hover:bg-surface-800" : "",
        ].join(" ")}
      >
        <StatusDot status={status} neutral={showNeutral} />
        <span className="text-text-dim">{icon}</span>
        <span className="text-[11px] uppercase tracking-wider text-text-dim">{label}</span>
        <span className="min-w-0 flex-1 truncate font-mono text-xs text-text-secondary">{primary}</span>
        {meta}
        {showToolDurations && startedAt && <DurationLabel startedAt={startedAt} endedAt={endedAt} />}
        {!showNeutral && <StatusBadge status={status} />}
        {onToggle && navigate && <ArrowUpRight className="h-3.5 w-3.5 text-text-dim" />}
        {onToggle && !navigate && (
          <ChevronDown
            className={["h-3.5 w-3.5 text-text-dim transition-transform", expanded ? "rotate-180" : ""].join(" ")}
          />
        )}
      </Header>
      {subBody}
      {expanded && body}
    </div>
  );
}

function DurationLabel({ startedAt, endedAt }: { startedAt: string; endedAt?: string }) {
  const running = !endedAt;
  const [now, setNow] = useState(() => Date.now());
  useEffect(() => {
    if (!running) return;
    const id = setInterval(() => setNow(Date.now()), 1000);
    return () => clearInterval(id);
  }, [running]);
  const start = Date.parse(startedAt);
  if (!Number.isFinite(start)) return null;
  const end = endedAt ? Date.parse(endedAt) : now;
  if (!Number.isFinite(end)) return null;
  const text = formatDurationMs(Math.max(0, end - start));
  const caveat =
    "counts from the agent's first tool_call frame, which can fire before the subprocess actually starts (upstream limitation)";
  return (
    <span
      className="text-[11px] text-text-dim tabular-nums"
      title={running ? `running ${text}; ${caveat}` : `${text}; ${caveat}`}
    >
      {text}
    </span>
  );
}

export function formatDurationMs(ms: number): string {
  if (ms < 1000) return `${ms} ms`;
  if (ms < 60_000) return `${(ms / 1000).toFixed(1)}s`;
  const totalSec = Math.floor(ms / 1000);
  return `${Math.floor(totalSec / 60)}m ${totalSec % 60}s`;
}

/** `45s`, `3m 14s`, `1h 7m`, `2d 4h`, dropping a zero remainder. */
export function formatDurationSeconds(seconds: number): string {
  const s = Math.max(0, Math.floor(seconds));
  const pair = (whole: number, unit: string, rem: number, remUnit: string) =>
    rem === 0 ? `${whole}${unit}` : `${whole}${unit} ${rem}${remUnit}`;
  if (s < 60) return `${s}s`;
  const m = Math.floor(s / 60);
  if (m < 60) return pair(m, "m", s % 60, "s");
  const h = Math.floor(m / 60);
  if (h < 24) return pair(h, "h", m % 60, "m");
  return pair(Math.floor(h / 24), "d", h % 24, "h");
}

function CopyButton({ text }: { text: string }) {
  return (
    <button
      type="button"
      title="Copy"
      onClick={(e) => {
        e.stopPropagation();
        navigator.clipboard?.writeText(text).catch(() => {});
      }}
      className="rounded p-1 text-text-dim hover:bg-surface-800 hover:text-text-secondary"
    >
      <CopyIcon className="h-3 w-3" />
    </button>
  );
}

/** Labelled, copyable raw text block ("input" / "output"). */
export function RawBlock({ label, text }: { label: "input" | "output"; text: string }) {
  return (
    <div className="border-t border-surface-800 bg-surface-950 px-3 py-2">
      <div className="mb-1 flex items-center justify-between text-[10px] uppercase tracking-wider text-text-dim">
        <span>{label}</span>
        <CopyButton text={text} />
      </div>
      <pre
        className={`overflow-x-auto font-mono text-[11px] ${label === "input" ? "text-text-muted" : "text-text-secondary"} whitespace-pre-wrap break-all`}
      >
        {text}
      </pre>
    </div>
  );
}

export function PlaceholderLine({ children }: { children: React.ReactNode }) {
  return (
    <div className="border-t border-surface-800 bg-surface-950 px-3 py-2 text-[11px] text-text-dim italic">
      {children}
    </div>
  );
}

// Tool output is agent-controlled: never let a `javascript:` uri reach a sink.
// Media `src` only takes schemes the browser loads over http.
export const SAFE_LINK_SCHEMES = new Set(["http:", "https:", "file:", "mailto:"]);
export const SAFE_MEDIA_SCHEMES = new Set(["http:", "https:"]);

export function safeUri(uri: string, schemes: ReadonlySet<string>): string | null {
  try {
    return schemes.has(new URL(uri, window.location.href).protocol) ? uri : null;
  } catch {
    return null;
  }
}

/** Strip a single outer markdown fence, returning its language hint. */
export function unwrapMarkdownFence(text: string): { text: string; lang: string | null } {
  const m = text.match(/^```([\w+-]+)?\s*\n([\s\S]*?)\n```\s*$/);
  if (!m) return { text, lang: null };
  return { text: m[2] ?? "", lang: m[1] ?? null };
}

export function HighlightedBlock({
  text,
  language,
  maxLines = 20,
}: {
  text: string;
  language?: string;
  maxLines?: number;
}) {
  // Keyed by the inputs that produced it, so a superseded request resolving
  // before its effect cleanup renders nothing. Theme is left out of the key so
  // a theme switch keeps the old palette until the re-highlight lands.
  // NUL-delimited (as the escape sequence: a raw NUL byte in source makes git
  // treat the file as binary) so field concatenations cannot collide.
  const [result, setResult] = useState<{ key: string; html: string } | null>(null);
  const [showAll, setShowAll] = useState(false);
  const shiki = useShikiTheme();
  const unwrapped = unwrapMarkdownFence(text);
  const effectiveLang = unwrapped.lang ?? language;
  const lines = unwrapped.text.split("\n");
  const limit = showAll ? 1_000_000 : maxLines;
  const shown = lines.length <= limit ? unwrapped.text : lines.slice(0, limit).join("\n");
  const truncated = Math.max(0, lines.length - limit);
  const inputKey = `${effectiveLang ?? ""}\u0000${shown}`;
  // Shiki cannot highlight SGR escapes, so ANSI output renders styled spans instead.
  const ansi = hasAnsi(shown);

  useEffect(() => {
    if (ansi) return;
    let cancelled = false;
    if (!effectiveLang) return;
    (async () => {
      try {
        const out = await highlightSnippet(shown, {
          langHint: effectiveLang,
          theme: shiki.theme,
          appearance: shiki.appearance,
        });
        if (cancelled || !out) return;
        setResult({ key: inputKey, html: out });
      } catch {
        // Unknown language: stay plain.
      }
    })();
    return () => {
      cancelled = true;
    };
  }, [effectiveLang, shown, inputKey, shiki.theme, shiki.appearance, ansi]);

  const html = result && result.key === inputKey ? result.html : null;

  return (
    <div className="border-t border-surface-800 bg-surface-950">
      {ansi ? (
        <AnsiBlock text={shown} />
      ) : html ? (
        <div
          className="overflow-x-auto px-3 py-2 text-xs [&_pre]:!bg-transparent [&_pre]:!m-0 [&_pre]:!p-0"
          dangerouslySetInnerHTML={{ __html: html }}
        />
      ) : (
        <pre className="overflow-x-auto px-3 py-2 text-xs font-mono text-text-secondary whitespace-pre-wrap break-all">
          {shown}
        </pre>
      )}
      {truncated > 0 && (
        <button
          type="button"
          onClick={() => setShowAll(true)}
          className="block w-full border-t border-surface-800 px-3 py-1 text-center text-[11px] text-text-dim hover:bg-surface-800"
        >
          Show {truncated} more line{truncated === 1 ? "" : "s"}
        </button>
      )}
    </div>
  );
}

/** Terminal output is column-sensitive, so no wrapping. */
function AnsiBlock({ text }: { text: string }) {
  const segments = useMemo(() => parseAnsi(text), [text]);
  return (
    <pre className="overflow-x-auto px-3 py-2 text-xs font-mono text-text-primary whitespace-pre">
      {segments.map((seg, i) => {
        const href = seg.url ? safeUri(seg.url, SAFE_LINK_SCHEMES) : null;
        return href ? (
          <a
            key={i}
            href={href}
            target="_blank"
            rel="noopener noreferrer"
            style={ansiSegmentStyle(seg.style)}
            className="underline"
          >
            {seg.text}
          </a>
        ) : (
          <span key={i} style={ansiSegmentStyle(seg.style)}>
            {seg.text}
          </span>
        );
      })}
    </pre>
  );
}

function ansiSegmentStyle(style: AnsiStyle): CSSProperties {
  const fg = style.inverse ? style.bg : style.fg;
  const bg = style.inverse ? style.fg : style.bg;
  return {
    color: fg,
    backgroundColor: bg,
    fontWeight: style.bold ? 600 : undefined,
    fontStyle: style.italic ? "italic" : undefined,
    textDecoration: style.underline ? "underline" : undefined,
    opacity: style.dim ? 0.65 : undefined,
  };
}

/** Header file path; opens the in-app viewer when a handler exists, without toggling the card. */
export function ClickableFilePath({ rawPath, display }: { rawPath: string; display: React.ReactNode }) {
  const { onOpenFileRef } = useAcpFileRef();
  if (!onOpenFileRef || rawPath === "(unknown file)") {
    return <span title={rawPath}>{display}</span>;
  }
  return (
    <button
      type="button"
      title={rawPath}
      onClick={(e) => {
        e.stopPropagation();
        onOpenFileRef({ path: rawPath });
      }}
      className="cursor-pointer text-left hover:text-text-primary hover:underline"
    >
      {display}
    </button>
  );
}
