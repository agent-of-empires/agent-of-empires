// A sub-agent's tool calls as compact rows that open the regular tool card.

import {
  ChevronRight,
  FileText,
  Globe,
  HelpCircle,
  Pencil,
  Search,
  Sparkles,
  Terminal,
  Trash2,
  type LucideIcon,
} from "lucide-react";
import { useMemo, useState } from "react";

import { formatDurationMs } from "../ToolCardChrome";
import { ToolCard } from "../ToolCards";
import {
  applyFilter,
  foldActivity,
  foldLabel,
  followTail,
  kindLabel,
  type ActivityFilter,
  type ActivityNode,
} from "./activityFold";
import type { ActivityTool, ToolStatus } from "./model";

const KIND_ICONS: Record<string, LucideIcon> = {
  execute: Terminal,
  read: FileText,
  edit: Pencil,
  delete: Trash2,
  search: Search,
  fetch: Globe,
  agent: Sparkles,
};

const GLYPHS: Record<ToolStatus, { char: string; cls: string; word: string }> = {
  running: { char: "●", cls: "text-status-waiting animate-pulse", word: "running" },
  ok: { char: "✓", cls: "text-status-running", word: "ok" },
  err: { char: "✕", cls: "text-status-error", word: "failed" },
  stopped: { char: "■", cls: "text-text-dim", word: "stopped" },
};

function duration(t: ActivityTool): string | null {
  if (!t.startedAt || !t.endedAt) return null;
  const ms = Date.parse(t.endedAt) - Date.parse(t.startedAt);
  return Number.isFinite(ms) && ms >= 0 ? formatDurationMs(ms) : null;
}

function Row({ tool }: { tool: ActivityTool }) {
  const [toggled, setToggled] = useState<boolean | null>(null);
  // A call that fails while shown opens itself unless the reader chose otherwise.
  const open = toggled ?? (tool.status === "err" && !!tool.call);
  const Icon = KIND_ICONS[tool.kind] ?? HelpCircle;
  const glyph = GLYPHS[tool.status];
  const time = duration(tool);
  return (
    <div data-activity-status={tool.status}>
      <button
        type="button"
        aria-expanded={open}
        onClick={() => setToggled(!open)}
        title={tool.target ? `${tool.name} ${tool.target}` : tool.name}
        className="flex h-7 w-full items-center gap-2 px-2 text-left text-xs hover:bg-surface-800"
      >
        <span aria-label={glyph.word} role="img" className={`w-3 shrink-0 text-center ${glyph.cls}`}>
          {glyph.char}
        </span>
        <Icon className="h-3.5 w-3.5 shrink-0 text-text-dim" />
        <span className="shrink-0 font-mono text-text-secondary">{tool.name}</span>
        <span className="min-w-0 flex-1 truncate font-mono text-text-dim">{tool.target}</span>
        {time && <span className="shrink-0 tabular-nums text-[11px] text-text-dim">{time}</span>}
      </button>
      {open && (
        <div className="border-l border-surface-700 pl-2 ml-3">
          {tool.call ? (
            <ToolCard tool={tool.call.tool} result={tool.call.result} nested />
          ) : (
            <p className="whitespace-pre-wrap break-all px-2 py-1 font-mono text-[11px] text-text-dim">
              {tool.detail ?? tool.target ?? tool.name}
            </p>
          )}
        </div>
      )}
    </div>
  );
}

function Fold({ node, dashed }: { node: Extract<ActivityNode, { type: "fold" }>; dashed?: boolean }) {
  const [open, setOpen] = useState(false);
  const span = node.shape === "span" ? ` · ${node.tools.length} hidden` : "";
  return (
    <div>
      <button
        type="button"
        aria-expanded={open}
        onClick={() => setOpen((v) => !v)}
        className={[
          "flex h-7 w-full items-center gap-2 px-2 text-left text-xs text-text-dim hover:bg-surface-800",
          dashed ? "border border-dashed border-surface-700" : "",
        ].join(" ")}
      >
        <ChevronRight className={`h-3 w-3 shrink-0 transition-transform ${open ? "rotate-90" : ""}`} />
        <span>
          {dashed ? "Earlier: " : ""}
          {foldLabel(node)} · no errors{dashed ? "" : span}
        </span>
      </button>
      {open && node.tools.map((t) => <Row key={t.id} tool={t} />)}
    </div>
  );
}

function Nodes({ nodes, dashedFirstFold }: { nodes: ActivityNode[]; dashedFirstFold?: boolean }) {
  return (
    <>
      {nodes.map((n, i) =>
        n.type === "row" ? (
          <Row key={n.tool.id} tool={n.tool} />
        ) : (
          <Fold key={n.tools[0]!.id} node={n} dashed={dashedFirstFold && i === 0} />
        ),
      )}
    </>
  );
}

const CHIP = "rounded px-1.5 py-0.5 text-[11px]";

export function ActivityFilters({
  tools,
  filter,
  onChange,
}: {
  tools: ActivityTool[];
  filter: ActivityFilter;
  onChange: (f: ActivityFilter) => void;
}) {
  const kinds = useMemo(() => [...new Set(tools.map((t) => t.kind))], [tools]);
  const errors = tools.filter((t) => t.status === "err").length;
  const active = (f: ActivityFilter) =>
    f === filter || (typeof f === "object" && typeof filter === "object" && f.kind === filter.kind);
  const chip = (label: string, f: ActivityFilter) => (
    <button
      key={label}
      type="button"
      aria-pressed={active(f)}
      onClick={() => onChange(f)}
      className={`${CHIP} ${active(f) ? "bg-surface-700 text-text-primary" : "text-text-dim hover:text-text-secondary"}`}
    >
      {label}
    </button>
  );
  return (
    <div className="flex flex-wrap items-center gap-0.5" role="group" aria-label="Filter activity">
      {chip("All", "all")}
      {errors > 0 && chip(`Errors ${errors}`, "errors")}
      {kinds.length > 1 && kinds.map((k) => chip(kindLabel(k, 2), { kind: k }))}
    </div>
  );
}

export function ActivityRows({
  tools,
  filter,
  following,
  running,
}: {
  tools: ActivityTool[];
  filter: ActivityFilter;
  running: boolean;
  /** A running agent: only the newest rows, the earlier ones folded. */
  following: boolean;
}) {
  const nodes = useMemo(() => {
    const shown = applyFilter(tools, filter);
    return following && filter === "all" ? followTail(shown) : foldActivity(shown);
  }, [tools, filter, following]);
  if (tools.length === 0) {
    const text = running ? "No tool calls yet." : "Tool calls were not retained.";
    return <p className="px-2 py-1.5 text-xs text-text-dim">{text}</p>;
  }
  if (nodes.length === 0) return <p className="px-2 py-1.5 text-xs text-text-dim">No calls match this filter.</p>;
  return <Nodes nodes={nodes} dashedFirstFold={following && filter === "all"} />;
}
