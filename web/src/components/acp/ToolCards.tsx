/* eslint-disable react-refresh/only-export-components */
// Per-kind tool call renderers. Each component takes the started tool
// and (optionally) the completion row, and renders a card that fits
// the shape of the tool's inputs and outputs.
//
// Patterns inspired by Cursor agent chat and VSCode Copilot Chat: each
// tool feels purpose-built rather than a generic "tool ran" box. We
// surface the key fields (path, command, query) inline in the card
// header and put output in a syntax-highlighted body.

import {
  useCallback,
  useEffect,
  useMemo,
  useState,
  type CSSProperties,
  type ReactNode,
  type SetStateAction,
} from "react";
import {
  Activity,
  Brain,
  Calendar,
  CalendarPlus,
  ArrowUpRight,
  CalendarX,
  ChevronDown,
  Clock,
  Copy as CopyIcon,
  FileDown,
  FileText,
  Globe,
  Image as ImageIcon,
  Layers,
  Link as LinkIcon,
  ListChecks,
  Music,
  Pencil,
  Plug,
  Search,
  Sparkles,
  Square,
  Terminal,
  Trash2,
} from "lucide-react";

import { ensureThemeLoaded, getHighlighter, langKeyForExt, loadLanguage } from "../../lib/highlighter";
import { useShikiTheme } from "../../hooks/useShikiTheme";
import { hasAnsi, parseAnsi, type AnsiStyle } from "../../lib/ansi";
import { parseJsonObject, pickFirst, pickStr, todoItemsFromArgs } from "../../lib/acpArgs";
import { useAcpPrefs } from "../../lib/acpPrefs";
import type { ActivityRow, ToolCall, ToolOutputBlock } from "../../lib/acpTypes";
import { diffPair } from "../../lib/diffPair";
import { StringDiff } from "../diff/StringDiff";
import { ToolErrorBody } from "./ToolErrorBody";
import { classifyMcp, humanizeServer, humanizeVerb } from "../../lib/mcpClassify";
import { cleanRecalledMemory, classifyMemory, parseMemoryFrontmatter, type MemoryHit } from "../../lib/memoryClassify";
import DOMPurify from "dompurify";
import { marked } from "marked";
import { reclassifyBash } from "../../lib/toolReclassify";
import { useAgentProfile } from "../../lib/agentProfileContext";
import { useAcpFileRef } from "./AcpFileRefContext";
import { useAcpSessionId } from "../../lib/acpSessionContext";
import { PluginToolCardBadges } from "../plugin/PluginSlots";
import { useBackgroundAgentFor, useOpenBackgroundAgentsPane } from "./backgroundAgentsContext";
import { relativeDisplayPath } from "../../lib/fileRef";
import { useToolDisplayMode, type ToolDensity } from "./ToolDisplayMode";
import type { AgentProfile, CardKind } from "../../lib/agentProfiles";

interface Props {
  tool: ToolCall;
  result?: ActivityRow;
}

/** Keys AcpRuntime smuggles through `args_preview` for renderer
 *  bookkeeping (the ACP title, the real `started_at` for the duration
 *  label, the sub-agent parent tool-call id). Excluded from any
 *  user-visible input JSON dumps. */
function isAcpBookkeepingKey(key: string): boolean {
  return key === "_aoe_title" || key === "_aoe_started_at" || key === "_aoe_parent_tool_call_id";
}

/** Read the smuggled `_aoe_parent_tool_call_id` from a tool's
 *  args_preview. Present when the tool is a Claude sub-agent (Task)
 *  child; falsy on top-level tools. See #1041. */
function hasSubagentParent(tool: ToolCall): boolean {
  const args = parseJsonObject(tool.args_preview);
  return Boolean(pickStr(args, "_aoe_parent_tool_call_id"));
}

interface ToolCardProps extends Props {
  /** True when rendered inside a SubagentCard body so the dispatcher
   *  doesn't re-wrap the child in the indented "↳ subagent" frame
   *  (the SubagentCard's own border already conveys the linkage). */
  nested?: boolean;
}

export function ToolCard({ tool, result, nested }: ToolCardProps) {
  const profile = useAgentProfile();
  const card = renderToolCard(tool, result, profile);
  // Structured media/resources the agent shipped at completion render
  // below the per-kind card, regardless of tool kind. See #1818.
  const media = result?.output?.length ? <ToolOutputMedia blocks={result.output} /> : null;
  const content = media ? (
    <>
      {card}
      {media}
    </>
  ) : (
    card
  );
  if (!nested && hasSubagentParent(tool)) {
    return <SubagentChildWrap>{content}</SubagentChildWrap>;
  }
  return content;
}

/** Render a tool call's structured completion payload (#1818). Images and
 *  audio render inline (preferring an embedded base64 payload, falling back
 *  to a `uri`); resource links and binary resources become download links;
 *  text and text-resources render as plain blocks. A media block with no
 *  inline data and no uri degrades to a labelled placeholder so the output
 *  is never silently dropped. */
function ToolOutputMedia({ blocks }: { blocks: ToolOutputBlock[] }) {
  return (
    <div className="my-1 space-y-2 overflow-hidden rounded-md border border-surface-700 bg-surface-800/50 p-3 text-sm">
      {blocks.map((block, i) => (
        <ToolOutputBlockView key={i} block={block} />
      ))}
    </div>
  );
}

function dataUri(mimeType: string, data: string): string {
  return `data:${mimeType};base64,${data}`;
}

// Tool output uris are agent-controlled, so a `javascript:` uri must never
// reach the DOM where a click could execute it. The allowlist is split by
// sink: clickable links accept the broader set (a `mailto:` link is valid,
// a `mailto:` image is not), while media `src` values accept only schemes a
// browser can actually load over the dashboard's http origin. See #1818
// review.
const SAFE_LINK_SCHEMES = new Set(["http:", "https:", "file:", "mailto:"]);
const SAFE_MEDIA_SCHEMES = new Set(["http:", "https:"]);

/** Return `uri` unchanged when its scheme is in `schemes` (resolved against
 *  the current origin so relative paths keep working), else null. */
function safeUri(uri: string, schemes: ReadonlySet<string>): string | null {
  try {
    const parsed = new URL(uri, window.location.href);
    return schemes.has(parsed.protocol) ? uri : null;
  } catch {
    return null;
  }
}

function MediaPlaceholder({ icon, label }: { icon: ReactNode; label: string }) {
  return (
    <div className="flex items-center gap-2 text-[11px] text-text-dim italic">
      <span className="text-text-dim">{icon}</span>
      {label}
    </div>
  );
}

function ToolOutputBlockView({ block }: { block: ToolOutputBlock }) {
  switch (block.kind) {
    case "text":
      return <pre className="whitespace-pre-wrap break-words font-mono text-xs text-text-secondary">{block.text}</pre>;
    case "image": {
      const src = block.data
        ? dataUri(block.mime_type, block.data)
        : block.uri
          ? safeUri(block.uri, SAFE_MEDIA_SCHEMES)
          : null;
      if (!src) {
        return <MediaPlaceholder icon={<ImageIcon className="h-3.5 w-3.5" />} label={`image (${block.mime_type})`} />;
      }
      return (
        <img
          src={src}
          alt={`tool output image (${block.mime_type})`}
          className="max-h-80 max-w-full rounded border border-surface-700 object-contain"
        />
      );
    }
    case "audio": {
      if (!block.data) {
        return <MediaPlaceholder icon={<Music className="h-3.5 w-3.5" />} label={`audio (${block.mime_type})`} />;
      }
      return <audio controls src={dataUri(block.mime_type, block.data)} className="w-full" />;
    }
    case "resource_link": {
      const href = safeUri(block.uri, SAFE_LINK_SCHEMES);
      if (!href) {
        return <MediaPlaceholder icon={<LinkIcon className="h-3.5 w-3.5" />} label={`${block.name} (${block.uri})`} />;
      }
      return (
        <a
          href={href}
          target="_blank"
          rel="noreferrer"
          className="flex items-center gap-2 text-xs text-accent-500 hover:underline"
        >
          <LinkIcon className="h-3.5 w-3.5" />
          {block.name}
        </a>
      );
    }
    case "resource": {
      if (block.text != null) {
        return (
          <pre className="whitespace-pre-wrap break-words font-mono text-xs text-text-secondary">{block.text}</pre>
        );
      }
      // A binary (blob) resource ships its bytes inline; offer them as a
      // download even when the uri isn't fetchable. See #1818.
      if (block.data) {
        const filename = block.uri.split("/").pop() || "resource";
        return (
          <a
            href={dataUri(block.mime_type ?? "application/octet-stream", block.data)}
            download={filename}
            className="flex items-center gap-2 text-xs text-accent-500 hover:underline"
          >
            <FileDown className="h-3.5 w-3.5" />
            {filename}
          </a>
        );
      }
      const href = safeUri(block.uri, SAFE_LINK_SCHEMES);
      if (!href) {
        return <MediaPlaceholder icon={<FileDown className="h-3.5 w-3.5" />} label={block.uri} />;
      }
      return (
        <a
          href={href}
          target="_blank"
          rel="noreferrer"
          className="flex items-center gap-2 text-xs text-accent-500 hover:underline"
        >
          <FileDown className="h-3.5 w-3.5" />
          {block.uri}
        </a>
      );
    }
    default:
      return null;
  }
}

function renderToolCard(tool: ToolCall, result: ActivityRow | undefined, profile: AgentProfile) {
  // claude-agent-acp v0.37.0+ routes session-start memory recall
  // through the tool channel with structured metadata (upstream #703).
  // Render the dedicated card before falling through to the path-sniff
  // MemoryCard so adapters that emit the structured shape don't end up
  // double-classified.
  if (tool.memory_recall) {
    return <MemoryRecallCard tool={tool} result={result} />;
  }
  const memory = classifyMemory(tool);
  if (memory.isMemory) {
    return <MemoryCard tool={tool} result={result} hit={memory} />;
  }
  const mcp = classifyMcp(tool, profile);
  if (mcp.isMcp) {
    return <McpToolCard tool={tool} result={result} server={mcp.server} verb={mcp.verb} />;
  }
  if (profile.capabilities.skills) {
    const skill = classifySkill(tool, profile);
    if (skill.isSkill) {
      return <SkillToolCard tool={tool} result={result} skillName={skill.name} />;
    }
  }
  if (profile.capabilities.todos) {
    const todos = classifyTodoWrite(tool, profile);
    if (todos.isTodoWrite) {
      return <TodoUpdateCard tool={tool} result={result} todos={todos.todos} />;
    }
  }
  if (profile.capabilities.wakeup) {
    const schedule = classifySchedule(tool, profile);
    if (schedule.kind) {
      return <ScheduleToolCard tool={tool} result={result} kind={schedule.kind} />;
    }
  }
  const harness = classifyHarnessTool(tool, profile);
  if (harness.kind) {
    return <HarnessToolCard tool={tool} result={result} kind={harness.kind} />;
  }
  const { kind, provenance } = reclassifyBash(tool);
  const effectiveKind = resolveEffectiveKind(tool, kind, profile);
  switch (effectiveKind) {
    case "execute":
      return <ExecuteToolCard tool={tool} result={result} />;
    case "read":
      return <ReadToolCard tool={tool} result={result} />;
    case "edit":
      return <EditToolCard tool={tool} result={result} />;
    case "delete":
      return <DeleteToolCard tool={tool} result={result} />;
    case "search":
      return <SearchToolCard tool={tool} result={result} provenance={provenance} />;
    case "fetch":
      return <FetchToolCard tool={tool} result={result} />;
    case "think":
      return <ThinkToolCard tool={tool} result={result} />;
    default:
      return <GenericToolCard tool={tool} result={result} />;
  }
}

/** Resolve the dispatch kind for a tool call. Trusts the ACP `kind`
 *  when it's a concrete card category; for `"other"` or unrecognised
 *  kinds, consults the active agent profile's alias table so adapter
 *  tools that don't take advantage of `ToolKind` (codex `shell`,
 *  gemini `run_shell_command`, etc.) still land on the right card. */
function resolveEffectiveKind(tool: ToolCall, reclassifiedKind: string, profile: AgentProfile): string {
  const known: ReadonlySet<string> = new Set(["execute", "read", "edit", "delete", "search", "fetch", "think"]);
  if (known.has(reclassifiedKind)) {
    return reclassifiedKind;
  }
  const name = tool.name?.trim() ?? "";
  if (!name) return reclassifiedKind;
  for (const [card, aliases] of Object.entries(profile.aliases) as [CardKind, string[]][]) {
    if (aliases.some((alias) => alias === name)) {
      return card;
    }
  }
  return reclassifiedKind;
}

/** Indented wrap that marks a tool card as a sub-agent (Claude Task)
 *  child. Keeps the activity feed flat (no tree restructuring yet,
 *  see #1041 layer B) but gives the user a scannable cue that the
 *  call belongs to a sub-task. */
function SubagentChildWrap({ children }: { children: ReactNode }) {
  return (
    <div className="border-l-2 border-accent-600/60 pl-2 ml-1">
      <div className="mb-0.5 inline-flex items-center gap-1 text-[10px] uppercase tracking-wider text-accent-600">
        <span>↳</span>
        <span>subagent</span>
      </div>
      {children}
    </div>
  );
}

/* ── Shared header bits ──────────────────────────────────────────── */

type Status = "running" | "ok" | "err" | "stopped";

function statusFor(result?: ActivityRow): Status {
  if (!result) return "running";
  if (result.kind === "tool_error") return "err";
  if (result.kind === "tool_stopped") return "stopped";
  return "ok";
}

// Per-card expand state for tool cards. Failed cards auto-open so the
// user sees the error immediately, but the chevron must still fold them
// once read. We model the user's choice as an override scoped to the
// active tool density: while no override matches the current density we
// derive openness from the baseline, so both a replayed already-failed
// card (mounts as err) and a live card that fails mid-stream
// (running -> err) open during render with no effect and no one-frame
// collapse flash. The first user toggle pins the override and is
// respected from then on, even if the card later re-enters err. See
// #1467.
//
// The baseline folds in the transcript density (#1767): "compact" makes
// every card default collapsed, "detailed" keeps the caller's
// `defaultOpen`. Errored cards always auto-open regardless of density so
// compact mode never hides a failure. Scoping the override to the
// density means flipping the global toggle re-applies the baseline for
// every untouched card without a useEffect.
function useToolCardExpansion(status: Status, defaultOpen = false) {
  const density = useToolDisplayMode();
  const baseline = status === "err" ? true : density === "compact" ? false : defaultOpen;
  const [override, setOverride] = useState<{
    density: ToolDensity;
    open: boolean;
  } | null>(null);
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
  // Group cards opt into a neutral dot once every child has settled
  // because a single child error doesn't make the whole group "failed"
  // and rolling errors up overstates the signal. The actionable status
  // stays visible on the per-child cards inside the expanded body.
  // See #1102.
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
  if (status === "stopped") {
    // `--color-status-stopped` fails WCAG AA for text (it is a dot-only
    // decorative token); the muted dot already differentiates from the
    // green "done" dot, so keep the label on the legible dim slot.
    return <span className="text-[11px] text-text-dim">stopped</span>;
  }
  return <span className="text-[11px] text-text-dim">done</span>;
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
  /** Region rendered unconditionally below the header, regardless of
   *  `expanded`. Used by `TodoGroupCard` to keep the latest todo list
   *  visible while the per-snapshot history stays behind the expand
   *  toggle (#1468). */
  subBody?: React.ReactNode;
  /** When true and the card has settled (`status !== "running"`),
   *  render a neutral dot and omit the status badge. Used by
   *  `ToolGroupCard` so a single child error doesn't roll up to a
   *  red header for the whole group; the actionable error signal
   *  stays on the per-child cards inside the expanded body. See
   *  #1102. */
  neutralOnDone?: boolean;
  /** When the header click navigates somewhere (e.g. opens a pane)
   *  rather than expanding the card, show an up-right arrow instead of
   *  the disclosure chevron so the row doesn't read as collapsible. */
  navigate?: boolean;
  /** ISO-8601 start timestamp for the underlying tool call. When set
   *  with `endedAt` (completed call) or alone (in-flight call), the
   *  header shows a duration label next to the status badge (#1060).
   *
   *  Rendering is gated by the `acp.show_tool_durations` setting
   *  (resolved server-side from `[acp]` in config.toml, surfaced
   *  via `ServerAbout.acp_show_tool_durations`, consumed here via
   *  `useAcpPrefs`). Default on; cross-device because the setting
   *  lives in the daemon's config file rather than the browser.
   *
   *  IMPORTANT; the measurement is imprecise on claude-agent-acp.
   *  The adapter emits each ACP `tool_call` frame at the wall time
   *  the model streams its tool_use chunk, which is typically well
   *  before the Claude Code SDK dispatches the subprocess; it also
   *  never emits `status: "in_progress"` so we cannot re-stamp
   *  `started_at` to the real subprocess start. Parallel
   *  `sleep 1` / `sleep 2` / `sleep 5` therefore render as
   *  ~3s / ~3.5s / ~6s instead of ~1s / ~2s / ~5s; durations
   *  include stream-arrival skew rather than just runtime. Once
   *  upstream gains a trustworthy "subprocess started" signal
   *  (either a `status: in_progress` frame or a `_meta` flag), the
   *  existing re-stamp path in `acp_client::map_update_to_events`
   *  picks it up with no further change here. The setting lets users
   *  hide the label in the meantime if the inflated numbers are more
   *  confusing than useful. */
  startedAt?: string;
  /** ISO-8601 timestamp from the matching `tool_complete` /
   *  `tool_error` row. Absent → tool still running, duration ticks
   *  live. */
  endedAt?: string;
}

function CardChrome({
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
  const settled = status !== "running";
  const showNeutral = neutralOnDone === true && settled;
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

/** Render `started_at → ended_at` as a human duration. While the tool
 *  is still running the label ticks once a second so users see the
 *  elapsed time grow. Tooltip names the known measurement
 *  imprecision (see `CardChromeProps.startedAt`) so users who notice
 *  "sleep 1 took 3s" find the explanation in-place. */
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
  const ms = Math.max(0, end - start);
  const text = formatDurationMs(ms);
  const tooltip = running
    ? `running ${text}; counts from the agent's first tool_call frame, which can fire before the subprocess actually starts (upstream limitation)`
    : `${text}; counts from the agent's first tool_call frame, which can fire before the subprocess actually starts (upstream limitation)`;
  return (
    <span className="text-[11px] text-text-dim tabular-nums" title={tooltip}>
      {text}
    </span>
  );
}

export function formatDurationMs(ms: number): string {
  if (ms < 1000) return `${ms} ms`;
  if (ms < 60_000) return `${(ms / 1000).toFixed(1)}s`;
  const totalSec = Math.floor(ms / 1000);
  const m = Math.floor(totalSec / 60);
  const s = totalSec % 60;
  return `${m}m ${s}s`;
}

/* ── Helpers ─────────────────────────────────────────────────────── */

function truncateLines(
  text: string,
  max: number,
): {
  shown: string;
  truncated: number;
} {
  const lines = text.split("\n");
  if (lines.length <= max) return { shown: text, truncated: 0 };
  return {
    shown: lines.slice(0, max).join("\n"),
    truncated: lines.length - max,
  };
}

function copy(text: string) {
  navigator.clipboard?.writeText(text).catch(() => {});
}

function CopyButton({ text }: { text: string }) {
  return (
    <button
      type="button"
      title="Copy"
      onClick={(e) => {
        e.stopPropagation();
        copy(text);
      }}
      className="rounded p-1 text-text-dim hover:bg-surface-800 hover:text-text-secondary"
    >
      <CopyIcon className="h-3 w-3" />
    </button>
  );
}

/* ── Highlighted code block (used by Read, Edit, Execute output) ── */

/** If the input is a single outer markdown code fence (```lang ... ```),
 *  strip the fence and return the inner body plus the fence's language
 *  hint. Tool output emitted by ACP agents (Claude in particular) is
 *  routinely pre-wrapped in fenced blocks like ```console ...```; left
 *  un-stripped, the cards render literal backticks above the content. */
function unwrapMarkdownFence(text: string): {
  text: string;
  lang: string | null;
} {
  const m = text.match(/^```([\w+-]+)?\s*\n([\s\S]*?)\n```\s*$/);
  if (!m) return { text, lang: null };
  return { text: m[2] ?? "", lang: m[1] ?? null };
}

function HighlightedBlock({ text, language, maxLines = 20 }: { text: string; language?: string; maxLines?: number }) {
  const [html, setHtml] = useState<string | null>(null);
  const [showAll, setShowAll] = useState(false);
  const shiki = useShikiTheme();
  const unwrapped = unwrapMarkdownFence(text);
  const effectiveText = unwrapped.text;
  const effectiveLang = unwrapped.lang ?? language;
  const { shown, truncated } = truncateLines(effectiveText, showAll ? 1_000_000 : maxLines);

  // ANSI fast path: when the text carries SGR escape sequences (e.g.
  // `gls --color=always`, `git status --color=always`), Shiki's bash
  // grammar can't handle them; it would either render the literal
  // `[01;34m` noise or fail to highlight at all. Render the styled
  // segments directly instead.
  const ansi = hasAnsi(shown);

  useEffect(() => {
    if (ansi) return;
    let cancelled = false;
    if (!effectiveLang) return;
    (async () => {
      try {
        const langKey = langKeyForExt(effectiveLang) ?? effectiveLang;
        await loadLanguage(langKey);
        const resolvedTheme = await ensureThemeLoaded(shiki.theme, shiki.appearance);
        const hl = await getHighlighter();
        if (cancelled) return;
        const out = hl.codeToHtml(shown, {
          lang: langKey,
          theme: resolvedTheme,
        });
        setHtml(out);
      } catch {
        // unknown language; fall back to plain
      }
    })();
    return () => {
      cancelled = true;
    };
  }, [effectiveLang, shown, shiki.theme, shiki.appearance, ansi]);

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

/** Render text with embedded ANSI SGR codes as styled spans. We use
 *  `whitespace-pre` (not `pre-wrap`) because terminal output is
 *  column-sensitive; wrapping mangles tabular layouts like `ps aux`
 *  or `df -h`. */
function AnsiBlock({ text }: { text: string }) {
  const segments = useMemo(() => parseAnsi(text), [text]);
  return (
    <pre className="overflow-x-auto px-3 py-2 text-xs font-mono text-text-primary whitespace-pre">
      {segments.map((seg, i) => (
        <span key={i} style={ansiSegmentStyle(seg.style)}>
          {seg.text}
        </span>
      ))}
    </pre>
  );
}

function ansiSegmentStyle(style: AnsiStyle): CSSProperties {
  // Inverse swaps fg/bg before applying.
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

/* ── execute (bash) ─────────────────────────────────────────────── */

function ExecuteToolCard({ tool, result }: Props) {
  const status = statusFor(result);
  const args = parseJsonObject(tool.args_preview);
  const argCommand = pickStr(args, "command", "cmd", "args");
  // Fallback chain: real command → ACP-provided title (forwarded via
  // _aoe_title in AcpRuntime) → tool's own kind/name. Never show
  // the literal `{}` from an empty raw_input.
  const title = pickStr(args, "_aoe_title");
  const command = pickFirst(argCommand, title, tool.name) ?? "(no command)";
  const description = pickStr(args, "description");
  const output = result?.text ?? "";
  const [open, setOpen] = useToolCardExpansion(status);

  const meta =
    output && status !== "running" ? (
      <span className="hidden md:inline text-[11px] text-text-dim">
        {unwrapMarkdownFence(output).text.split("\n").length} lines
      </span>
    ) : undefined;

  return (
    <CardChrome
      status={status}
      startedAt={tool.started_at}
      endedAt={result?.at}
      icon={<Terminal className="h-3.5 w-3.5" />}
      label="bash"
      primary={
        <>
          <span className="mr-1 text-text-dim">$</span>
          {command}
        </>
      }
      meta={meta}
      expanded={open}
      onToggle={() => setOpen((v) => !v)}
      body={
        <ToolErrorBody status={status} errorText={result?.text}>
          {description && (
            <div className="border-t border-surface-800 bg-surface-900/40 px-3 py-1 text-[11px] text-text-muted italic">
              {description}
            </div>
          )}
          {/* Full command; the chrome's primary slot is single-line
              truncated, so we surface the untruncated command here so
              users can read and copy it. Shiki's bash grammar gives
              the same coloring as our markdown code blocks. */}
          <HighlightedBlock text={command} language="bash" maxLines={6} />
          {output && status !== "err" ? (
            <HighlightedBlock text={output} language="bash" maxLines={20} />
          ) : status !== "err" ? (
            <div className="border-t border-surface-800 bg-surface-950 px-3 py-2 text-[11px] text-text-dim italic">
              {status === "running" ? "Running…" : "(no output)"}
            </div>
          ) : null}
        </ToolErrorBody>
      }
    />
  );
}

/* ── read ───────────────────────────────────────────────────────── */

function ReadToolCard({ tool, result }: Props) {
  const status = statusFor(result);
  const { fileRefSession } = useAcpFileRef();
  const args = parseJsonObject(tool.args_preview);
  const argPath = pickStr(args, "path", "file_path", "filePath", "filename");
  const title = pickStr(args, "_aoe_title");
  const rawPath = pickFirst(argPath, title, tool.name) ?? "(unknown file)";
  const path = relativeDisplayPath(rawPath, fileRefSession);
  const range = formatRange(args);
  const ext = argPath?.match(/\.([a-z0-9]+)$/i)?.[1]?.toLowerCase();
  const content = result?.text ?? "";
  const [open, setOpen] = useToolCardExpansion(status);

  const meta = content && (
    <span className="hidden md:inline text-[11px] text-text-dim">{content.split("\n").length} lines</span>
  );

  return (
    <CardChrome
      status={status}
      startedAt={tool.started_at}
      endedAt={result?.at}
      icon={<FileText className="h-3.5 w-3.5" />}
      label="read"
      primary={<span title={rawPath}>{path}</span>}
      meta={
        <>
          {range && <span className="text-[11px] text-text-dim">{range}</span>}
          {meta}
        </>
      }
      expanded={open}
      onToggle={status === "err" || content ? () => setOpen((v) => !v) : undefined}
      body={
        <ToolErrorBody status={status} errorText={result?.text}>
          {content && status !== "err" && <HighlightedBlock text={content} language={ext} maxLines={16} />}
        </ToolErrorBody>
      }
    />
  );
}

function formatRange(args: Record<string, unknown> | null): string | null {
  if (!args) return null;
  const offset = typeof args.offset === "number" ? args.offset : null;
  const limit = typeof args.limit === "number" ? args.limit : null;
  if (offset !== null && limit !== null) return `L${offset}–${offset + limit}`;
  if (offset !== null) return `from L${offset}`;
  if (limit !== null) return `${limit} lines`;
  return null;
}

/* ── edit / write ───────────────────────────────────────────────── */

function EditToolCard({ tool, result }: Props) {
  const status = statusFor(result);
  const { fileRefSession } = useAcpFileRef();
  const args = parseJsonObject(tool.args_preview);
  const argPath = pickStr(args, "path", "file_path", "filePath", "filename");
  const title = pickStr(args, "_aoe_title");
  // Codex (and any ACP agent) attaches structured per-file diffs via
  // ToolCallContent::Diff; prefer those for path + body and fall back to
  // the legacy old_string/new_string args shape otherwise. See #1721.
  const structuredDiffs = tool.diffs ?? [];
  const hasStructuredDiffs = structuredDiffs.length > 0;
  const legacyOld = pickStr(args, "old_string", "oldString", "old_str") ?? "";
  const legacyNew = pickStr(args, "new_string", "newString", "new_str", "content") ?? "";
  const hasLegacyDiff = legacyOld !== "" || legacyNew !== "";
  const rawPath = pickFirst(structuredDiffs[0]?.path, argPath, title, tool.name) ?? "(unknown file)";
  const path = relativeDisplayPath(rawPath, fileRefSession);
  const [open, setOpen] = useToolCardExpansion(status);
  const hasDiff = hasStructuredDiffs || hasLegacyDiff;
  // "edit" when a prior version existed, "write" for a fresh file.
  const isEdit = hasStructuredDiffs ? structuredDiffs.some((d) => (d.old_text ?? "") !== "") : legacyOld !== "";
  const verb = isEdit ? "edit" : "write";
  const multiFile = structuredDiffs.length > 1;

  const { adds, dels } = useMemo(() => {
    const ds = tool.diffs ?? [];
    if (ds.length > 0) {
      return ds.reduce(
        (acc, d) => {
          const p = diffPair(d.old_text ?? "", d.new_text ?? "");
          return { adds: acc.adds + p.adds, dels: acc.dels + p.dels };
        },
        { adds: 0, dels: 0 },
      );
    }
    return diffPair(legacyOld, legacyNew);
  }, [tool.diffs, legacyOld, legacyNew]);
  const meta = hasDiff && (adds > 0 || dels > 0) && (
    <span className="hidden md:inline text-[11px]">
      <span className="text-emerald-400">+{adds}</span> <span className="text-rose-400">−{dels}</span>
    </span>
  );

  // Hide the "+N -M" chip on failure: no change actually landed, and
  // the chip reads as a successful diff summary. Surface the adapter's
  // failure reason via ToolErrorBody instead. See #1090.
  const errorChip = status === "err";
  return (
    <CardChrome
      status={status}
      startedAt={tool.started_at}
      endedAt={result?.at}
      icon={<Pencil className="h-3.5 w-3.5" />}
      label={verb}
      primary={<span title={rawPath}>{multiFile ? `${path} +${structuredDiffs.length - 1} more` : path}</span>}
      meta={errorChip ? undefined : meta}
      expanded={open}
      onToggle={status === "err" || hasDiff ? () => setOpen((v) => !v) : undefined}
      body={
        <ToolErrorBody status={status} errorText={result?.text}>
          {hasStructuredDiffs ? (
            <div className="border-t border-surface-800 bg-surface-950">
              {structuredDiffs.map((d, i) => (
                <div key={`${d.path}-${i}`}>
                  {multiFile && (
                    <div className="px-2 py-1 text-[11px] text-text-dim" title={d.path}>
                      {relativeDisplayPath(d.path, fileRefSession)}
                    </div>
                  )}
                  <StringDiff oldText={d.old_text ?? ""} newText={d.new_text ?? ""} filePath={d.path} />
                </div>
              ))}
            </div>
          ) : (
            hasLegacyDiff && (
              <div className="border-t border-surface-800 bg-surface-950">
                <StringDiff oldText={legacyOld} newText={legacyNew} filePath={path} />
              </div>
            )
          )}
        </ToolErrorBody>
      }
    />
  );
}

/* ── delete ─────────────────────────────────────────────────────── */

function DeleteToolCard({ tool, result }: Props) {
  const status = statusFor(result);
  const { fileRefSession } = useAcpFileRef();
  const args = parseJsonObject(tool.args_preview);
  const argPath = pickStr(args, "path", "file_path", "filePath", "filename");
  const title = pickStr(args, "_aoe_title");
  const rawPath = pickFirst(argPath, title, tool.name) ?? "(unknown file)";
  const path = relativeDisplayPath(rawPath, fileRefSession);
  const [open, setOpen] = useToolCardExpansion(status);
  return (
    <CardChrome
      status={status}
      startedAt={tool.started_at}
      endedAt={result?.at}
      icon={<Trash2 className="h-3.5 w-3.5 text-rose-400" />}
      label="delete"
      primary={<span title={rawPath}>{path}</span>}
      expanded={open}
      onToggle={status === "err" ? () => setOpen((v) => !v) : undefined}
      body={
        status === "err" ? (
          <ToolErrorBody status={status} errorText={result?.text}>
            {null}
          </ToolErrorBody>
        ) : undefined
      }
    />
  );
}

/* ── search ─────────────────────────────────────────────────────── */

interface SearchProps extends Props {
  /** Set to "bash" when the call was a grep/find/rg shell-out that the
   *  dispatcher reclassified into this card. Surfaced in the label so
   *  the swap stays transparent ("search · bash"). */
  provenance?: "bash" | null;
}

function SearchToolCard({ tool, result, provenance }: SearchProps) {
  const status = statusFor(result);
  const args = parseJsonObject(tool.args_preview);
  const argQuery = pickStr(args, "query", "pattern", "q", "search");
  const argCommand = pickStr(args, "command");
  const title = pickStr(args, "_aoe_title");
  const query = pickFirst(argQuery, title, argCommand, tool.name) ?? "(no query)";
  const path = pickStr(args, "path", "directory", "scope");
  const output = result?.text ?? "";
  const lines = output ? output.split("\n").filter(Boolean) : [];
  const [open, setOpen] = useToolCardExpansion(status);

  return (
    <CardChrome
      status={status}
      startedAt={tool.started_at}
      endedAt={result?.at}
      icon={<Search className="h-3.5 w-3.5" />}
      label={provenance === "bash" ? "search · bash" : "search"}
      primary={query}
      meta={
        <>
          {path && <span className="hidden md:inline text-[11px] text-text-dim">in {path}</span>}
          {lines.length > 0 && (
            <span className="text-[11px] text-text-dim">
              {lines.length} match{lines.length === 1 ? "" : "es"}
            </span>
          )}
        </>
      }
      expanded={open}
      onToggle={status === "err" || lines.length > 0 ? () => setOpen((v) => !v) : undefined}
      body={
        <ToolErrorBody status={status} errorText={result?.text}>
          {lines.length > 0 && status !== "err" && (
            <div className="border-t border-surface-800 bg-surface-950 max-h-64 overflow-y-auto">
              {lines.slice(0, 50).map((l, i) => (
                <div key={i} className="flex font-mono text-[11px] hover:bg-surface-900">
                  <span className="select-none w-10 shrink-0 px-2 py-0.5 text-right text-text-dim">{i + 1}</span>
                  <span className="px-2 py-0.5 text-text-secondary truncate">{l}</span>
                </div>
              ))}
              {lines.length > 50 && (
                <div className="border-t border-surface-800 px-3 py-1 text-center text-[11px] text-text-dim">
                  {lines.length - 50} more match
                  {lines.length - 50 === 1 ? "" : "es"}
                </div>
              )}
            </div>
          )}
        </ToolErrorBody>
      }
    />
  );
}

/* ── fetch ──────────────────────────────────────────────────────── */

function FetchToolCard({ tool, result }: Props) {
  const status = statusFor(result);
  const args = parseJsonObject(tool.args_preview);
  const argUrl = pickStr(args, "url", "uri", "endpoint");
  const title = pickStr(args, "_aoe_title");
  const url = pickFirst(argUrl, title, tool.name) ?? "(no url)";
  const output = result?.text ?? "";
  const [open, setOpen] = useToolCardExpansion(status);

  return (
    <CardChrome
      status={status}
      startedAt={tool.started_at}
      endedAt={result?.at}
      icon={<Globe className="h-3.5 w-3.5" />}
      label="fetch"
      primary={url}
      expanded={open}
      onToggle={status === "err" || output ? () => setOpen((v) => !v) : undefined}
      body={
        <ToolErrorBody status={status} errorText={result?.text}>
          {output && status !== "err" && <HighlightedBlock text={output} language="json" maxLines={16} />}
        </ToolErrorBody>
      }
    />
  );
}

/* ── think ──────────────────────────────────────────────────────── */

function ThinkToolCard({ tool }: Props) {
  return (
    <div className="my-1 flex items-center gap-2 px-3 py-1 text-xs italic text-text-muted">
      <Sparkles className="h-3 w-3 text-text-dim" />
      <span>{tool.name || "thinking…"}</span>
    </div>
  );
}

/* ── todowrite ──────────────────────────────────────────────────── */

type TodoStatus = "pending" | "in_progress" | "completed" | "cancelled";

interface TodoItem {
  content: string;
  status: TodoStatus;
}

/** Heuristic for agent todo tools. Claude and OpenCode both carry the
 *  current list in `args_preview.todos`, but their titles differ. The
 *  profile opt-in is the safety gate; the structured todo payload is the
 *  shared discriminator so grouping and rendering agree. See #1905. */
function classifyTodoWrite(
  tool: ToolCall,
  profile: AgentProfile,
): { isTodoWrite: true; todos: TodoItem[] } | { isTodoWrite: false } {
  if (!profile.capabilities.todos) return { isTodoWrite: false };
  const args = parseJsonObject(tool.args_preview);
  // Array-presence, not item count: an empty `todos: []` is a real
  // clear-list snapshot, while a non-todo think tool carries no `todos`
  // key at all. Gating on length rejected legitimate clears. See #2003.
  if (!Array.isArray(args?.todos)) return { isTodoWrite: false };
  const payload = todoItemsFromArgs(args);
  const todos = payload.map((entry) => ({
    content: entry.content,
    status: normaliseTodoStatus(entry.status),
  }));
  return { isTodoWrite: true, todos };
}

function normaliseTodoStatus(raw: unknown): TodoStatus {
  const s = typeof raw === "string" ? raw.toLowerCase() : "";
  if (s === "in_progress" || s === "in-progress" || s === "active") {
    return "in_progress";
  }
  if (s === "completed" || s === "complete" || s === "done") return "completed";
  if (s === "cancelled" || s === "canceled" || s === "abandoned") {
    return "cancelled";
  }
  return "pending";
}

interface TodoCardProps extends Props {
  todos: TodoItem[];
}

const TODO_GLYPH: Record<TodoStatus, string> = {
  pending: "☐",
  in_progress: "▶",
  completed: "✓",
  cancelled: "⊘",
};

const TODO_CLASS: Record<TodoStatus, string> = {
  pending: "text-text-secondary",
  in_progress: "text-brand-400",
  completed: "text-emerald-400 line-through opacity-70",
  cancelled: "text-text-dim line-through",
};

function todoCounts(todos: TodoItem[]): Record<TodoStatus, number> {
  const c: Record<TodoStatus, number> = {
    pending: 0,
    in_progress: 0,
    completed: 0,
    cancelled: 0,
  };
  for (const t of todos) c[t.status] += 1;
  return c;
}

function todoBreakdown(counts: Record<TodoStatus, number>): string[] {
  const breakdown: string[] = [];
  if (counts.in_progress > 0) breakdown.push(`${counts.in_progress} active`);
  if (counts.pending > 0) breakdown.push(`${counts.pending} pending`);
  if (counts.completed > 0) breakdown.push(`${counts.completed} done`);
  if (counts.cancelled > 0) breakdown.push(`${counts.cancelled} cancelled`);
  return breakdown;
}

/** The todo checklist itself, shared by the per-call `TodoUpdateCard`
 *  body and the folded `TodoGroupCard`'s always-visible latest list. */
function TodoList({ todos }: { todos: TodoItem[] }) {
  if (todos.length === 0) {
    return (
      <div className="border-t border-surface-800 bg-surface-950 px-3 py-2 font-mono text-xs text-text-dim">
        todos cleared
      </div>
    );
  }
  return (
    <div className="border-t border-surface-800 bg-surface-950 px-3 py-2">
      <ul className="flex flex-col gap-1 font-mono text-xs">
        {todos.map((t, i) => (
          <li key={`${i}-${t.content}`} className={`flex items-start gap-2 ${TODO_CLASS[t.status]}`}>
            <span className="select-none w-4 shrink-0 text-center">{TODO_GLYPH[t.status]}</span>
            <span className="min-w-0 flex-1 whitespace-pre-wrap break-words">{t.content}</span>
          </li>
        ))}
      </ul>
    </div>
  );
}

function TodoUpdateCard({ tool, result, todos }: TodoCardProps) {
  const status = statusFor(result);
  const counts = useMemo(() => todoCounts(todos), [todos]);
  const [open, setOpen] = useToolCardExpansion(status, todos.length <= 5);
  const breakdown = todoBreakdown(counts);

  return (
    <CardChrome
      status={status}
      icon={<ListChecks className="h-3.5 w-3.5" />}
      label="todos"
      primary={
        <>
          <span>{todos.length === 0 ? "todos cleared" : `${todos.length} items`}</span>
          {breakdown.length > 0 && <span className="ml-2 text-text-dim">· {breakdown.join(" · ")}</span>}
        </>
      }
      expanded={open}
      onToggle={() => setOpen((v) => !v)}
      startedAt={tool.started_at}
      endedAt={result?.at}
      body={
        <ToolErrorBody status={status} errorText={result?.text}>
          <TodoList todos={todos} />
        </ToolErrorBody>
      }
    />
  );
}

interface TodoGroupChild {
  tool: ToolCall;
  result?: ActivityRow;
}

/** Folds a run of consecutive TodoWrite snapshots into one card. The
 *  collapsed view shows the latest list (the only snapshot whose
 *  pending/in_progress/completed mix is current); expanding reveals
 *  each individual update in order so the plan's evolution during the
 *  turn stays auditable. See #1468. */
export function TodoGroupCard({ items }: { items: TodoGroupChild[] }) {
  const profile = useAgentProfile();
  const [open, setOpen] = useState(false);
  const snapshots = useMemo(
    () =>
      items
        .map((it) => {
          const c = classifyTodoWrite(it.tool, profile);
          return c.isTodoWrite ? { tool: it.tool, result: it.result, todos: c.todos } : null;
        })
        .filter(
          (
            s,
          ): s is {
            tool: ToolCall;
            result: ActivityRow | undefined;
            todos: TodoItem[];
          } => s !== null,
        ),
    [items, profile],
  );
  if (snapshots.length === 0) return null;

  // The collapsed preview shows the latest *successful* snapshot, since
  // a TodoWrite that ended in tool_error (or was interrupted by a stop,
  // tool_stopped; see #1646) never became the live state. The header
  // still reflects the latest attempt so a failed trailing update
  // surfaces as an error rather than looking clean. See #1468.
  const latestAttempt = snapshots[snapshots.length - 1]!;
  const latestSuccessful =
    [...snapshots].reverse().find((s) => s.result?.kind !== "tool_error" && s.result?.kind !== "tool_stopped") ?? null;
  const previewSnapshot = latestSuccessful ?? latestAttempt;
  const latestFailed = latestAttempt.result?.kind === "tool_error";
  const latestStopped = latestAttempt.result?.kind === "tool_stopped";
  const breakdown = todoBreakdown(todoCounts(previewSnapshot.todos));
  const running = snapshots.some((s) => !s.result);
  const status: Status = running ? "running" : latestFailed ? "err" : latestStopped ? "stopped" : "ok";

  const startedAt = snapshots
    .map((s) => s.tool.started_at)
    .sort()
    .at(0);
  const allDone = snapshots.every((s) => s.result);
  const endedAt = allDone
    ? snapshots
        .map((s) => s.result!.at)
        .sort()
        .at(-1)
    : undefined;

  return (
    <CardChrome
      status={status}
      neutralOnDone={!latestFailed && !latestStopped}
      icon={<ListChecks className="h-3.5 w-3.5" />}
      label="todos"
      primary={
        <>
          <span>updated {snapshots.length} times</span>
          {breakdown.length > 0 && <span className="ml-2 text-text-dim">· {breakdown.join(" · ")}</span>}
        </>
      }
      expanded={open}
      onToggle={() => setOpen((v) => !v)}
      startedAt={startedAt}
      endedAt={endedAt}
      subBody={previewSnapshot.result?.kind === "tool_error" ? undefined : <TodoList todos={previewSnapshot.todos} />}
      body={
        <div className="border-t border-surface-800 bg-surface-900/30 px-2 py-1">
          {snapshots.map((s) => (
            <TodoUpdateCard key={s.tool.id} tool={s.tool} result={s.result} todos={s.todos} />
          ))}
        </div>
      }
    />
  );
}

/* ── skill ──────────────────────────────────────────────────────── */

/** Heuristic for Claude's Skill tool, which the adapter routes through
 *  the generic "Other" arm so it arrives as `kind: "other"` with a bare
 *  `Skill` title and the skill identifier hidden in `args.skill`. We
 *  reclassify on (case-insensitive) name + args presence so the structured view
 *  shows what skill ran without making the user expand a JSON blob.
 *  See #1062. */
function classifySkill(tool: ToolCall, profile: AgentProfile): { isSkill: true; name: string } | { isSkill: false } {
  if (tool.kind !== "other") return { isSkill: false };
  const title = tool.name?.trim().toLowerCase() ?? "";
  const names = profile.specialTitles.skillNames;
  if (!names.includes(title)) return { isSkill: false };
  const args = parseJsonObject(tool.args_preview);
  const name = pickStr(args, "skill", "name", "skill_name") ?? "skill";
  return { isSkill: true, name };
}

interface SkillProps extends Props {
  skillName: string;
}

function SkillToolCard({ tool, result, skillName }: SkillProps) {
  const status = statusFor(result);
  const sessionId = useAcpSessionId();
  const [open, setOpen] = useToolCardExpansion(status);
  // Memo on the raw string so downstream memos see a stable args reference
  // and don't recompute every render.
  const args = useMemo(() => parseJsonObject(tool.args_preview), [tool.args_preview]);
  const output = result?.text ?? "";

  // Pretty-printed input minus the bookkeeping _aoe_title field so the
  // user sees the actual skill arguments, not the adapter's title echo.
  const inputJson = useMemo<string>(() => {
    if (!args) return tool.args_preview;
    const rest: Record<string, unknown> = {};
    for (const [k, v] of Object.entries(args)) {
      if (isAcpBookkeepingKey(k)) continue;
      rest[k] = v;
    }
    return JSON.stringify(rest, null, 2);
  }, [args, tool.args_preview]);

  const hasBody = Boolean((args && Object.keys(args).length > 0) || output);

  return (
    <CardChrome
      status={status}
      icon={<Sparkles className="h-3.5 w-3.5" />}
      label="skill"
      primary={skillName}
      meta={sessionId && <PluginToolCardBadges sessionId={sessionId} kind="skill" target={skillName} />}
      expanded={open}
      onToggle={status === "err" || hasBody ? () => setOpen((v) => !v) : undefined}
      startedAt={tool.started_at}
      endedAt={result?.at}
      body={
        <ToolErrorBody status={status} errorText={result?.text}>
          {args && Object.keys(args).filter((k) => !isAcpBookkeepingKey(k)).length > 0 && (
            <div className="border-t border-surface-800 bg-surface-950 px-3 py-2">
              <div className="mb-1 flex items-center justify-between text-[10px] uppercase tracking-wider text-text-dim">
                <span>input</span>
                <CopyButton text={inputJson} />
              </div>
              <pre className="overflow-x-auto font-mono text-[11px] text-text-muted whitespace-pre-wrap break-all">
                {inputJson}
              </pre>
            </div>
          )}
          {output && status !== "err" && <HighlightedBlock text={output} language="markdown" maxLines={16} />}
        </ToolErrorBody>
      }
    />
  );
}

/* ── tool group ─────────────────────────────────────────────────── */

interface ToolGroupItem {
  tool: ToolCall;
  result?: ActivityRow;
  /** Raw `toolName` from assistant-ui (the ACP kind), used for the
   *  per-kind tally in the group header. */
  kind: string;
}

/** Collapsible block summarising a run of tool calls between agent
 *  text. The activity log is unchanged; this is presentation only,
 *  matching how the Claude Code CLI condenses silent investigation
 *  phases. See #1057. */
export function ToolGroupCard({ items }: { items: ToolGroupItem[] }) {
  const runningCount = items.filter((i) => !i.result).length;
  const errorCount = items.filter((i) => i.result && i.result.kind === "tool_error").length;
  // No err rollup on the group header. A single failed child in an
  // 11-step investigation doesn't make the whole investigation
  // failed; per-child status stays on the inner cards. See #1102.
  const status: Status = runningCount > 0 ? "running" : "ok";
  // Route through the shared hook (not a bare useState) so the group
  // folds with the transcript density toggle too. Called before the
  // empty-items guard to keep hook order stable. See #1767.
  const [open, setOpen] = useToolCardExpansion(status, false);
  if (items.length === 0) return null;

  const breakdown = summariseKinds(items, errorCount);

  // Group duration spans the earliest start across children → latest
  // completion. Still-running calls leave `endedAt` undefined so the
  // duration label ticks live until every child completes.
  const startedAt = items
    .map((i) => i.tool.started_at)
    .sort()
    .at(0);
  const allDone = items.every((i) => i.result);
  const endedAt = allDone
    ? items
        .map((i) => i.result!.at)
        .sort()
        .at(-1)
    : undefined;

  return (
    <CardChrome
      status={status}
      startedAt={startedAt}
      endedAt={endedAt}
      neutralOnDone
      icon={<Layers className="h-3.5 w-3.5" />}
      label="actions"
      primary={
        <>
          <span>{items.length} actions</span>
          {breakdown && <span className="ml-2 text-text-dim">· {breakdown}</span>}
        </>
      }
      expanded={open}
      onToggle={() => setOpen((v) => !v)}
      body={
        open && (
          <div className="border-t border-surface-800 bg-surface-900/30 px-2 py-1">
            {items.map((item) => (
              <ToolCard key={item.tool.id} tool={item.tool} result={item.result} />
            ))}
          </div>
        )
      }
    />
  );
}

/* ── subagent ───────────────────────────────────────────────────── */

interface SubagentChildItem {
  tool: ToolCall;
  result?: ActivityRow;
}

interface SubagentProps {
  tool: ToolCall;
  result?: ActivityRow;
  children: SubagentChildItem[];
}

/** Card for a Claude sub-agent (Task) and its child tool calls. The
 *  parent Task shows in the header; the body lists the children using
 *  the same ToolCard dispatch as top-level calls (with `nested=true`
 *  so the indented "↳ subagent" wrap doesn't double up). See #1041. */
/** Inline card for an async sub-agent launch (Claude `Task` with isAsync).
 *  The sub-agent runs off-protocol; the daemon tails its transcript and
 *  publishes a `BackgroundAgent` record, which we look up by the launching
 *  tool-call id to show live status, elapsed time, and activity, linking
 *  this card to its entry in the Background agents panel. Before the first
 *  tailer event arrives (or if the record is gone), it degrades to a
 *  neutral "runs in background" card. The SDK launch-marker body (which
 *  carries an internal agent id) is never rendered. */
export function AsyncSubagentCard({ tool }: { tool: ToolCall }) {
  const args = useMemo(() => parseJsonObject(tool.args_preview), [tool.args_preview]);
  const description = pickStr(args, "description", "_aoe_title") ?? tool.name ?? "Subagent task";
  const agent = useBackgroundAgentFor(tool.id);
  const openPane = useOpenBackgroundAgentsPane();

  const status: Status = !agent
    ? "ok"
    : agent.status === "running"
      ? "running"
      : agent.status === "completed"
        ? "ok"
        : agent.status === "error"
          ? "err"
          : "stopped"; // stalled / detached

  let metaText = "runs in background";
  if (agent) {
    if (agent.status === "running") {
      const tools = agent.toolCount > 0 ? `${agent.toolCount} ${agent.toolCount === 1 ? "tool" : "tools"}` : null;
      metaText = [tools, agent.lastTool].filter(Boolean).join(" · ") || "running in background";
    } else if (agent.status === "stalled") {
      metaText = "stalled in background";
    } else if (agent.status === "detached") {
      metaText = "detached";
    } else if (agent.status === "completed") {
      metaText = "finished in background";
    } else {
      metaText = agent.warning ?? "background agent error";
    }
  }

  return (
    <CardChrome
      status={status}
      neutralOnDone={!agent || agent.status === "stalled" || agent.status === "detached"}
      icon={<Sparkles className="h-3.5 w-3.5" />}
      label="subagent"
      startedAt={agent?.startedAt ?? tool.started_at}
      endedAt={agent?.endedAt ?? undefined}
      primary={<span className="truncate">{description}</span>}
      meta={<span className="text-[11px] text-text-dim">{metaText}</span>}
      expanded={false}
      onToggle={openPane}
      navigate={openPane !== undefined}
    />
  );
}

export function SubagentCard({ tool, result, children }: SubagentProps) {
  const [open, setOpen] = useState(false);

  const args = useMemo(() => parseJsonObject(tool.args_preview), [tool.args_preview]);
  const description = pickStr(args, "description", "_aoe_title") ?? tool.name ?? "Subagent task";

  const runningChildren = children.filter((c) => !c.result).length;
  const parentDone = result !== undefined;
  const parentErrored = result?.kind === "tool_error";
  // Only the parent Task's own error rolls up to the header. A child
  // tool inside a successful subagent run errored is the same noise
  // as #1102 spotted on tool groups; let the per-child card carry
  // the actionable signal instead of marking the whole subagent
  // "failed".
  const status: Status = !parentDone || runningChildren > 0 ? "running" : parentErrored ? "err" : "ok";

  // Span the earliest started_at across the parent and any children
  // (children typically start slightly after the parent) and the
  // latest completion. Mirrors ToolGroupCard so the duration label
  // reflects total subagent runtime.
  const startedAt = [tool.started_at, ...children.map((c) => c.tool.started_at)].sort().at(0);
  const allDone = parentDone && children.every((c) => c.result !== undefined);
  const endedAt = allDone
    ? [result?.at ?? null, ...children.map((c) => c.result?.at ?? null)]
        .filter((v): v is string => v !== null)
        .sort()
        .at(-1)
    : undefined;

  return (
    <CardChrome
      status={status}
      startedAt={startedAt}
      endedAt={endedAt}
      icon={<Sparkles className="h-3.5 w-3.5" />}
      label="subagent"
      primary={
        <>
          <span className="truncate">{description}</span>
          <span className="ml-2 text-text-dim">
            · {children.length} {children.length === 1 ? "tool" : "tools"}
          </span>
        </>
      }
      expanded={open}
      onToggle={() => setOpen((v) => !v)}
      body={
        open && (
          <div className="border-t border-surface-800 bg-surface-900/30 px-2 py-1">
            {children.length === 0 ? (
              <div className="px-2 py-1 text-[11px] text-text-dim">No tool calls recorded yet.</div>
            ) : (
              children.map((c) => <ToolCard key={c.tool.id} tool={c.tool} result={c.result} nested />)
            )}
          </div>
        )
      }
    />
  );
}

function summariseKinds(items: ToolGroupItem[], errorCount: number = 0): string | null {
  const counts = new Map<string, number>();
  for (const i of items) {
    const k = labelForKind(i.kind);
    counts.set(k, (counts.get(k) ?? 0) + 1);
  }
  if (counts.size === 0) return null;
  const entries = Array.from(counts.entries()).sort((a, b) => b[1] - a[1]);
  const kinds = entries.map(([k, n]) => `${k} ${n}`).join(" · ");
  // Append an error count when present. The group header drops its
  // err rollup (#1102), so this is the only collapsed-state surface
  // that still tells the user something went wrong inside.
  if (errorCount > 0) {
    return `${kinds} · ${errorCount} error${errorCount === 1 ? "" : "s"}`;
  }
  return kinds;
}

function labelForKind(kind: string): string {
  switch (kind) {
    case "execute":
      return "Bash";
    case "read":
      return "Read";
    case "edit":
      return "Edit";
    case "delete":
      return "Delete";
    case "search":
      return "Search";
    case "fetch":
      return "Fetch";
    case "think":
      return "Think";
    default:
      return kind.charAt(0).toUpperCase() + kind.slice(1);
  }
}

/* ── mcp ────────────────────────────────────────────────────────── */

interface McpProps extends Props {
  server: string;
  verb: string;
}

function McpToolCard({ tool, result, server, verb }: McpProps) {
  const status = statusFor(result);
  const sessionId = useAcpSessionId();
  const [open, setOpen] = useToolCardExpansion(status);
  // Memo on the raw string so downstream memos see a stable args reference
  // and don't recompute every render.
  const args = useMemo(() => parseJsonObject(tool.args_preview), [tool.args_preview]);
  const output = result?.text ?? "";

  // Pull a short single-field arg preview for the header so the user
  // can see what the call was about without expanding. Skip the
  // _aoe_title bookkeeping field; cap length so headers stay readable.
  const argPreview = useMemo<string | null>(() => {
    if (!args) return null;
    for (const [k, v] of Object.entries(args)) {
      if (isAcpBookkeepingKey(k)) continue;
      if (typeof v === "string" && v.length > 0) {
        const trimmed = v.length > 120 ? `${v.slice(0, 117)}…` : v;
        return `${k}: ${trimmed}`;
      }
    }
    return null;
  }, [args]);

  // Pretty-printed input, excluding the bookkeeping _aoe_title field
  // so the user sees the actual MCP arguments, not the adapter's
  // forwarded title.
  const inputJson = useMemo<string>(() => {
    if (!args) return tool.args_preview;
    const rest: Record<string, unknown> = {};
    for (const [k, v] of Object.entries(args)) {
      if (isAcpBookkeepingKey(k)) continue;
      rest[k] = v;
    }
    return JSON.stringify(rest, null, 2);
  }, [args, tool.args_preview]);

  const hasBody = Boolean((args && Object.keys(args).length > 0) || output);

  return (
    <CardChrome
      status={status}
      startedAt={tool.started_at}
      endedAt={result?.at}
      icon={<Plug className="h-3.5 w-3.5" />}
      label={`MCP · ${humanizeServer(server)}`}
      primary={
        <>
          {humanizeVerb(verb)}
          {argPreview && <span className="ml-2 text-text-dim">· {argPreview}</span>}
        </>
      }
      meta={sessionId && <PluginToolCardBadges sessionId={sessionId} kind="mcp" target={server} />}
      expanded={open}
      onToggle={status === "err" || hasBody ? () => setOpen((v) => !v) : undefined}
      body={
        <ToolErrorBody status={status} errorText={result?.text}>
          {args && Object.keys(args).filter((k) => !isAcpBookkeepingKey(k)).length > 0 && (
            <div className="border-t border-surface-800 bg-surface-950 px-3 py-2">
              <div className="mb-1 flex items-center justify-between text-[10px] uppercase tracking-wider text-text-dim">
                <span>input</span>
                <CopyButton text={inputJson} />
              </div>
              <pre className="overflow-x-auto font-mono text-[11px] text-text-muted whitespace-pre-wrap break-all">
                {inputJson}
              </pre>
            </div>
          )}
          {output && status !== "err" && <HighlightedBlock text={output} language="markdown" maxLines={24} />}
        </ToolErrorBody>
      }
    />
  );
}

/* ── memory ─────────────────────────────────────────────────────── */

interface MemoryCardProps extends Props {
  hit: MemoryHit;
}

/** Dedicated card for Claude's memory-system file ops. Memory lives
 *  under `~/.claude/projects/<slug>/memory/*.md` and the agent touches
 *  it via plain Read/Edit/Write, so the upstream tool kind is the same
 *  as any other file op. We branch on the path predicate in
 *  classifyMemory and render here. See issue #1071. */
function MemoryCard({ tool, result, hit }: MemoryCardProps) {
  const status = statusFor(result);
  const [open, setOpen] = useToolCardExpansion(status);

  const args = useMemo(() => parseJsonObject(tool.args_preview), [tool.args_preview]);

  const content = useMemo<string>(() => {
    if (hit.verb === "recalled") return result?.text ?? "";
    const fromArgs = pickStr(args, "new_string", "newString", "new_str", "content") ?? "";
    return fromArgs;
  }, [hit.verb, args, result?.text]);

  const parsed = useMemo(() => (content ? parseMemoryFrontmatter(content) : null), [content]);

  const verbLabel = hit.isIndex && hit.verb === "recalled" ? "read index" : hit.verb;
  const headerLabel = hit.isIndex ? "Memory index" : "Memory";

  const meta = parsed?.type && <span className="hidden md:inline text-[11px] text-text-dim">{parsed.type}</span>;

  const hasBody = Boolean(content);

  return (
    <CardChrome
      status={status}
      startedAt={tool.started_at}
      endedAt={result?.at}
      icon={<Brain className="h-3.5 w-3.5" />}
      label={headerLabel}
      primary={
        <>
          <span>{verbLabel}</span>
          <span className="ml-2 text-text-dim">· {hit.basename}</span>
        </>
      }
      meta={meta}
      expanded={open}
      onToggle={status === "err" || hasBody ? () => setOpen((v) => !v) : undefined}
      body={
        <ToolErrorBody status={status} errorText={result?.text}>
          {hasBody && parsed && status !== "err" ? (
            <div className="border-t border-surface-800 bg-surface-950">
              {(parsed.name || parsed.description || parsed.type) && (
                <dl className="grid grid-cols-[auto_1fr] gap-x-3 gap-y-0.5 px-3 py-2 text-[11px]">
                  {parsed.name && (
                    <>
                      <dt className="text-text-dim">name</dt>
                      <dd className="text-text-secondary">{parsed.name}</dd>
                    </>
                  )}
                  {parsed.type && (
                    <>
                      <dt className="text-text-dim">type</dt>
                      <dd className="text-text-secondary">{parsed.type}</dd>
                    </>
                  )}
                  {parsed.description && (
                    <>
                      <dt className="text-text-dim">description</dt>
                      <dd className="text-text-secondary">{parsed.description}</dd>
                    </>
                  )}
                </dl>
              )}
              {parsed.body && <HighlightedBlock text={parsed.body} language="markdown" maxLines={24} />}
            </div>
          ) : null}
        </ToolErrorBody>
      }
    />
  );
}

/* ── memory_recall (session-start memory load) ──────────────────── */

/** Dedicated card for the session-start memory recall claude-agent-acp
 *  routes through the tool channel in v0.37.0 (upstream
 *  agentclientprotocol/claude-agent-acp#703). Two modes:
 *
 *  - recall: SDK loaded one or more memory files into the agent's
 *    context. Render the list of paths so the user sees what the
 *    agent already knows about them.
 *  - synthesize: SDK summarised the memories into a single text body.
 *    Render the body verbatim.
 *
 *  Replaces the aoe#1071 path-sniff workaround that inferred memory
 *  loads from subsequent Read tool calls; that path only caught
 *  user-driven reads of memory files, never the session-start load
 *  the agent received before any prompt was sent. The structured tool
 *  call now makes the load visible. */
function MemoryRecallCard({ tool, result }: Props) {
  const status = statusFor(result);
  const recall = tool.memory_recall;
  const [open, setOpen] = useToolCardExpansion(status);

  if (!recall) {
    // Defensive: dispatcher only enters this branch when memory_recall
    // is set, but type narrowing requires the check.
    return <GenericToolCard tool={tool} result={result} />;
  }
  const paths = recall.paths ?? [];
  const synthesized = cleanRecalledMemory(recall.synthesized_text ?? "");
  const isSynthesize = recall.mode === "synthesize";
  // Render the cleaned memory body as markdown. We use `marked` rather
  // than the structured-view `<Markdown>` component because that one
  // requires the assistant-ui runtime provider (it throws "requires an
  // AuiProvider" outside the transcript), the same reason CommentMarkdown
  // exists. The `.acp-markdown` class reuses the panel's prose styling.
  // `marked` does not sanitize HTML, so the parsed output is run through
  // DOMPurify before it reaches dangerouslySetInnerHTML; the recalled
  // text is agent-surfaced and could carry an injected payload.
  const synthesizedHtml = isSynthesize ? DOMPurify.sanitize(marked.parse(synthesized, { async: false }) as string) : "";

  const primary = isSynthesize ? (
    <span>Synthesised memory</span>
  ) : (
    <>
      <span>Recalled</span>
      <span className="ml-2 text-text-dim">
        · {paths.length} {paths.length === 1 ? "memory" : "memories"}
      </span>
    </>
  );

  const hasBody = isSynthesize ? synthesized.length > 0 : paths.length > 0;

  return (
    <CardChrome
      status={status}
      startedAt={tool.started_at}
      endedAt={result?.at}
      icon={<Brain className="h-3.5 w-3.5" />}
      label="Memory recall"
      primary={primary}
      expanded={open}
      onToggle={status === "err" || hasBody ? () => setOpen((v) => !v) : undefined}
      body={
        <ToolErrorBody status={status} errorText={result?.text}>
          {status !== "err" && hasBody ? (
            <div className="border-t border-surface-800 bg-surface-950 px-3 py-2">
              {isSynthesize ? (
                <div
                  data-testid="memory-recall-synthesized"
                  className="acp-markdown break-words text-[11px] text-text-secondary"
                  dangerouslySetInnerHTML={{ __html: synthesizedHtml }}
                />
              ) : (
                <ul data-testid="memory-recall-paths" className="space-y-0.5 text-[11px] text-text-secondary">
                  {paths.map((p) => (
                    <li key={p} className="break-all font-mono">
                      {p}
                    </li>
                  ))}
                </ul>
              )}
            </div>
          ) : null}
        </ToolErrorBody>
      }
    />
  );
}

/* ── schedule (ScheduleWakeup / Cron* family) ───────────────────── */

/** Classify the Claude Agent SDK scheduling tools by their well-known
 *  names. claude-agent-acp routes them all through the generic `Other`
 *  arm with no structured kind, so we name-match. See #1091.
 *
 *  Returns `kind: null` for non-schedule tools; otherwise one of:
 *  - `"wakeup"`: ScheduleWakeup ({ delaySeconds, prompt, reason })
 *  - `"cron_create"`: CronCreate
 *  - `"cron_list"`: CronList
 *  - `"cron_delete"`: CronDelete
 */
function classifySchedule(
  tool: ToolCall,
  profile: AgentProfile,
): { kind: "wakeup" | "cron_create" | "cron_list" | "cron_delete" | null } {
  if (tool.kind !== "other") return { kind: null };
  const args = parseJsonObject(tool.args_preview);
  // ACP titles come through the `_aoe_title` smuggle; the tool's own
  // `name` is usually the same value but matching both keeps us robust
  // to upstream relabels (e.g. claude-agent-acp future kind handling).
  const title = (pickStr(args, "_aoe_title") ?? tool.name ?? "").trim();
  const allowed = profile.specialTitles.scheduleNames;
  if (!allowed.includes(title)) return { kind: null };
  switch (title) {
    case "ScheduleWakeup":
      return { kind: "wakeup" };
    case "CronCreate":
      return { kind: "cron_create" };
    case "CronList":
      return { kind: "cron_list" };
    case "CronDelete":
      return { kind: "cron_delete" };
    default:
      return { kind: null };
  }
}

/** Format `seconds` as a short human-readable duration: `45s`, `3m 14s`,
 *  `1h 7m`, `2d 4h`. Used in schedule card headers so users see "wake
 *  in 3m 14s" instead of `delaySeconds: 194`. */
function formatDurationSeconds(seconds: number): string {
  const s = Math.max(0, Math.floor(seconds));
  if (s < 60) return `${s}s`;
  const m = Math.floor(s / 60);
  if (m < 60) {
    const rem = s % 60;
    return rem === 0 ? `${m}m` : `${m}m ${rem}s`;
  }
  const h = Math.floor(m / 60);
  if (h < 24) {
    const rem = m % 60;
    return rem === 0 ? `${h}h` : `${h}h ${rem}m`;
  }
  const d = Math.floor(h / 24);
  const rem = h % 24;
  return rem === 0 ? `${d}d` : `${d}d ${rem}h`;
}

/** Format an absolute clock time as `HH:MM` (24h, local timezone) for
 *  the wake-at meta line. */
function formatClockTime(date: Date): string {
  const hh = String(date.getHours()).padStart(2, "0");
  const mm = String(date.getMinutes()).padStart(2, "0");
  return `${hh}:${mm}`;
}

interface ScheduleProps extends Props {
  kind: "wakeup" | "cron_create" | "cron_list" | "cron_delete";
}

function ScheduleToolCard({ tool, result, kind }: ScheduleProps) {
  const status = statusFor(result);
  const [open, setOpen] = useToolCardExpansion(status);
  const args = useMemo(() => parseJsonObject(tool.args_preview), [tool.args_preview]);
  const output = result?.text ?? "";

  // Hide the bookkeeping fields and (for wakeup) the `prompt` field:
  // it's either the `<<autonomous-loop-dynamic>>` sentinel or a repeat
  // of the user's prior input, never user-relevant in the card view.
  const inputJson = useMemo<string>(() => {
    if (!args) return tool.args_preview;
    const rest: Record<string, unknown> = {};
    for (const [k, v] of Object.entries(args)) {
      if (isAcpBookkeepingKey(k)) continue;
      if (kind === "wakeup" && k === "prompt") continue;
      rest[k] = v;
    }
    return JSON.stringify(rest, null, 2);
  }, [args, tool.args_preview, kind]);

  const hasRawInput = useMemo(() => {
    if (!args) return Boolean(tool.args_preview);
    return Object.keys(args).some((k) => !isAcpBookkeepingKey(k) && !(kind === "wakeup" && k === "prompt"));
  }, [args, tool.args_preview, kind]);

  let icon: React.ReactNode;
  let label: string;
  let primary: React.ReactNode;
  let meta: React.ReactNode = undefined;

  if (kind === "wakeup") {
    const delayRaw = args ? args["delaySeconds"] : undefined;
    const delaySeconds =
      typeof delayRaw === "number" ? delayRaw : typeof delayRaw === "string" ? Number(delayRaw) : NaN;
    const reason = pickStr(args, "reason");
    const started = Date.parse(tool.started_at);
    const wakeAt =
      Number.isFinite(started) && Number.isFinite(delaySeconds) ? new Date(started + delaySeconds * 1000) : null;
    icon = <Clock className="h-3.5 w-3.5" />;
    label = "scheduled wakeup";
    primary = (
      <span>
        {Number.isFinite(delaySeconds) ? `in ${formatDurationSeconds(delaySeconds)}` : "scheduled"}
        {reason ? <span className="text-text-dim">: {reason}</span> : null}
      </span>
    );
    if (wakeAt) {
      meta = (
        <span className="hidden md:inline text-[11px] text-text-dim tabular-nums">
          wakes at {formatClockTime(wakeAt)}
        </span>
      );
    }
  } else if (kind === "cron_create") {
    const schedule = pickStr(args, "schedule", "cron", "expression");
    const reason = pickStr(args, "reason");
    icon = <CalendarPlus className="h-3.5 w-3.5" />;
    label = "cron schedule created";
    primary = (
      <span>
        {schedule ? <span className="font-mono">{schedule}</span> : "schedule created"}
        {reason ? <span className="text-text-dim">: {reason}</span> : null}
      </span>
    );
  } else if (kind === "cron_list") {
    icon = <Calendar className="h-3.5 w-3.5" />;
    label = "cron schedules";
    primary = "list active schedules";
  } else {
    // cron_delete
    const id = pickStr(args, "id", "name");
    icon = <CalendarX className="h-3.5 w-3.5" />;
    label = "cron schedule deleted";
    primary = id ? <span className="font-mono">{id}</span> : "deleted";
  }

  const hasBody = hasRawInput || Boolean(output) || status === "err";

  return (
    <CardChrome
      status={status}
      icon={icon}
      label={label}
      primary={primary}
      meta={meta}
      expanded={open}
      onToggle={status === "err" || hasBody ? () => setOpen((v) => !v) : undefined}
      startedAt={tool.started_at}
      endedAt={result?.at}
      body={
        <ToolErrorBody status={status} errorText={result?.text}>
          {hasRawInput && (
            <div className="border-t border-surface-800 bg-surface-950 px-3 py-2">
              <div className="mb-1 flex items-center justify-between text-[10px] uppercase tracking-wider text-text-dim">
                <span>input</span>
                <CopyButton text={inputJson} />
              </div>
              <pre className="overflow-x-auto font-mono text-[11px] text-text-muted whitespace-pre-wrap break-all">
                {inputJson}
              </pre>
            </div>
          )}
          {output && status !== "err" && (
            <div className="border-t border-surface-800 bg-surface-950 px-3 py-2">
              <div className="mb-1 flex items-center justify-between text-[10px] uppercase tracking-wider text-text-dim">
                <span>output</span>
                <CopyButton text={output} />
              </div>
              <pre className="overflow-x-auto font-mono text-[11px] text-text-secondary whitespace-pre-wrap break-all">
                {output}
              </pre>
            </div>
          )}
        </ToolErrorBody>
      }
    />
  );
}

/* ── harness tools (ToolSearch / Monitor / TaskStop) ─────────────── */

/** Classify Claude's agent-runtime harness tools by their well-known
 *  names. Like the schedule family, claude-agent-acp routes them through
 *  the generic `Other` arm with no structured kind, so we name-match
 *  against the profile's `harnessNames` allowlist (empty for non-Claude
 *  agents, so a coincidental `Monitor` tool elsewhere stays generic).
 *  See #2139.
 *
 *  - `"tool_search"`: ToolSearch ({ query, max_results })
 *  - `"monitor"`: Monitor ({ description, command, timeout_ms, persistent })
 *  - `"task_stop"`: TaskStop ({ task_id })
 */
function classifyHarnessTool(
  tool: ToolCall,
  profile: AgentProfile,
): { kind: "tool_search" | "monitor" | "task_stop" | null } {
  if (tool.kind !== "other") return { kind: null };
  const args = parseJsonObject(tool.args_preview);
  const title = (pickStr(args, "_aoe_title") ?? tool.name ?? "").trim();
  if (!profile.specialTitles.harnessNames.includes(title)) return { kind: null };
  switch (title) {
    case "ToolSearch":
      return { kind: "tool_search" };
    case "Monitor":
      return { kind: "monitor" };
    case "TaskStop":
      return { kind: "task_stop" };
    default:
      return { kind: null };
  }
}

interface HarnessProps extends Props {
  kind: "tool_search" | "monitor" | "task_stop";
}

function HarnessToolCard({ tool, result, kind }: HarnessProps) {
  const status = statusFor(result);
  const [open, setOpen] = useToolCardExpansion(status);
  const args = useMemo(() => parseJsonObject(tool.args_preview), [tool.args_preview]);
  const output = result?.text ?? "";

  const inputJson = useMemo<string>(() => {
    if (!args) return tool.args_preview;
    const rest: Record<string, unknown> = {};
    for (const [k, v] of Object.entries(args)) {
      if (isAcpBookkeepingKey(k)) continue;
      rest[k] = v;
    }
    return JSON.stringify(rest, null, 2);
  }, [args, tool.args_preview]);

  const hasRawInput = useMemo(() => {
    if (!args) return Boolean(tool.args_preview);
    return Object.keys(args).some((k) => !isAcpBookkeepingKey(k));
  }, [args, tool.args_preview]);

  let icon: React.ReactNode;
  let label: string;
  let primary: React.ReactNode;
  let meta: React.ReactNode = undefined;

  if (kind === "tool_search") {
    const query = pickStr(args, "query");
    icon = <Search className="h-3.5 w-3.5" />;
    label = "tool search";
    primary = query ? <span className="font-mono">{query}</span> : "search tools";
  } else if (kind === "monitor") {
    const description = pickStr(args, "description");
    const persistentRaw = args ? args["persistent"] : undefined;
    const timeoutRaw = args ? args["timeout_ms"] : undefined;
    const timeoutMs =
      typeof timeoutRaw === "number" ? timeoutRaw : typeof timeoutRaw === "string" ? Number(timeoutRaw) : NaN;
    icon = <Activity className="h-3.5 w-3.5" />;
    label = "monitor";
    primary = description ? <span>{description}</span> : "background watch";
    const chip =
      persistentRaw === true
        ? "persistent"
        : Number.isFinite(timeoutMs)
          ? formatDurationSeconds(timeoutMs / 1000)
          : null;
    if (chip) {
      meta = <span className="hidden md:inline text-[11px] text-text-dim tabular-nums">{chip}</span>;
    }
  } else {
    // task_stop
    const taskId = pickStr(args, "task_id", "shell_id");
    icon = <Square className="h-3.5 w-3.5" />;
    label = "task stop";
    primary = taskId ? <span className="font-mono">{taskId}</span> : "stop task";
  }

  const hasBody = hasRawInput || Boolean(output) || status === "err";

  return (
    <CardChrome
      status={status}
      icon={icon}
      label={label}
      primary={primary}
      meta={meta}
      expanded={open}
      onToggle={status === "err" || hasBody ? () => setOpen((v) => !v) : undefined}
      startedAt={tool.started_at}
      endedAt={result?.at}
      body={
        <ToolErrorBody status={status} errorText={result?.text}>
          {hasRawInput && (
            <div className="border-t border-surface-800 bg-surface-950 px-3 py-2">
              <div className="mb-1 flex items-center justify-between text-[10px] uppercase tracking-wider text-text-dim">
                <span>input</span>
                <CopyButton text={inputJson} />
              </div>
              <pre className="overflow-x-auto font-mono text-[11px] text-text-muted whitespace-pre-wrap break-all">
                {inputJson}
              </pre>
            </div>
          )}
          {output && status !== "err" && (
            <div className="border-t border-surface-800 bg-surface-950 px-3 py-2">
              <div className="mb-1 flex items-center justify-between text-[10px] uppercase tracking-wider text-text-dim">
                <span>output</span>
                <CopyButton text={output} />
              </div>
              <pre className="overflow-x-auto font-mono text-[11px] text-text-secondary whitespace-pre-wrap break-all">
                {output}
              </pre>
            </div>
          )}
        </ToolErrorBody>
      }
    />
  );
}

/* ── generic fallback ───────────────────────────────────────────── */

function GenericToolCard({ tool, result }: Props) {
  const status = statusFor(result);
  const [open, setOpen] = useToolCardExpansion(status);
  const output = result?.text ?? "";
  return (
    <CardChrome
      status={status}
      startedAt={tool.started_at}
      endedAt={result?.at}
      icon={<Sparkles className="h-3.5 w-3.5" />}
      label={tool.kind || "tool"}
      primary={tool.name}
      expanded={open}
      onToggle={status === "err" || tool.args_preview || output ? () => setOpen((v) => !v) : undefined}
      body={
        <ToolErrorBody status={status} errorText={result?.text}>
          {tool.args_preview && (
            <div className="border-t border-surface-800 bg-surface-950 px-3 py-2">
              <div className="mb-1 flex items-center justify-between text-[10px] uppercase tracking-wider text-text-dim">
                <span>input</span>
                <CopyButton text={tool.args_preview} />
              </div>
              <pre className="overflow-x-auto font-mono text-[11px] text-text-muted whitespace-pre-wrap break-all">
                {tool.args_preview}
              </pre>
            </div>
          )}
          {output && status !== "err" && (
            <div className="border-t border-surface-800 bg-surface-950 px-3 py-2">
              <div className="mb-1 flex items-center justify-between text-[10px] uppercase tracking-wider text-text-dim">
                <span>output</span>
                <CopyButton text={output} />
              </div>
              <pre className="overflow-x-auto font-mono text-[11px] text-text-secondary whitespace-pre-wrap break-all">
                {output}
              </pre>
            </div>
          )}
        </ToolErrorBody>
      }
    />
  );
}
