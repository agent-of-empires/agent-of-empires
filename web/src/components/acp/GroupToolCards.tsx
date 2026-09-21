/* eslint-disable react-refresh/only-export-components */
// Cards that fold several tool calls: action runs and sub-agent tasks.

import { Layers, Sparkles } from "lucide-react";

import { pickStr } from "../../lib/acpArgs";
import type { BackgroundAgentStatus, ToolCall } from "../../lib/acpTypes";
import { useBackgroundAgentFor, useOpenBackgroundAgentsPane } from "./backgroundAgentsContext";
import {
  CardChrome,
  HighlightedBlock,
  PlaceholderLine,
  spanTimes,
  useToolArgs,
  useToolCardExpansion,
  type Status,
  type ToolCardProps,
} from "./ToolCardChrome";
import { ToolCard } from "./ToolCards";
import { ToolErrorBody } from "./ToolErrorBody";

const ICON = "h-3.5 w-3.5";

const KIND_LABELS: Record<string, string> = {
  execute: "Bash",
  read: "Read",
  edit: "Edit",
  delete: "Delete",
  search: "Search",
  fetch: "Fetch",
  think: "Think",
};

/** "Bash 3 · Read 2", plus an error count since the group header never shows failure. */
function summariseKinds(items: { kind: string }[], errorCount: number): string | null {
  const counts = new Map<string, number>();
  for (const { kind } of items) {
    const label = KIND_LABELS[kind] ?? kind.charAt(0).toUpperCase() + kind.slice(1);
    counts.set(label, (counts.get(label) ?? 0) + 1);
  }
  if (counts.size === 0) return null;
  const kinds = Array.from(counts.entries())
    .sort((a, b) => b[1] - a[1])
    .map(([k, n]) => `${k} ${n}`)
    .join(" · ");
  return errorCount > 0 ? `${kinds} · ${errorCount} error${errorCount === 1 ? "" : "s"}` : kinds;
}

function ChildCards({ items, nested }: { items: ToolCardProps[]; nested?: boolean }) {
  return (
    <div className="border-t border-surface-800 bg-surface-900/30 px-2 py-1">
      {items.map((c) => (
        <ToolCard key={c.tool.id} tool={c.tool} result={c.result} nested={nested} />
      ))}
    </div>
  );
}

/** A run of tool calls between agent text, condensed. A failed child does not fail the group. */
export function ToolGroupCard({ items }: { items: (ToolCardProps & { kind: string })[] }) {
  const errorCount = items.filter((i) => i.result && i.result.kind === "tool_error").length;
  const status: Status = items.some((i) => !i.result) ? "running" : "ok";
  const [open, setOpen] = useToolCardExpansion(status, false);
  if (items.length === 0) return null;
  const breakdown = summariseKinds(items, errorCount);

  return (
    <CardChrome
      status={status}
      {...spanTimes(items)}
      neutralOnDone
      icon={<Layers className={ICON} />}
      label="actions"
      primary={
        <>
          <span>{items.length} actions</span>
          {breakdown && <span className="ml-2 text-text-dim">· {breakdown}</span>}
        </>
      }
      expanded={open}
      onToggle={() => setOpen((v) => !v)}
      body={open && <ChildCards items={items} />}
    />
  );
}

function useTaskDescription(tool: ToolCall) {
  const args = useToolArgs(tool);
  return { args, description: pickStr(args, "description", "_aoe_title") ?? tool.name ?? "Subagent task" };
}

const ASYNC_STATUS: Record<BackgroundAgentStatus, [Status, string]> = {
  running: ["running", "running in background"],
  completed: ["ok", "finished in background"],
  error: ["err", "background agent error"],
  stalled: ["stopped", "stalled in background"],
  detached: ["stopped", "detached"],
};

/** Async sub-agent launch, linked to its live Background agents record by
 *  tool-call id. Neutral until the first tailer event arrives. */
export function AsyncSubagentCard({ tool }: { tool: ToolCall }) {
  const { description } = useTaskDescription(tool);
  const agent = useBackgroundAgentFor(tool.id);
  const openPane = useOpenBackgroundAgentsPane();

  const [status, fallbackMeta] = agent ? ASYNC_STATUS[agent.status] : (["ok", "runs in background"] as const);
  let metaText = fallbackMeta;
  if (agent?.status === "running") {
    const tools = agent.toolCount > 0 ? `${agent.toolCount} ${agent.toolCount === 1 ? "tool" : "tools"}` : null;
    metaText = [tools, agent.lastTool].filter(Boolean).join(" · ") || fallbackMeta;
  } else if (agent?.status === "error") {
    metaText = agent.warning ?? fallbackMeta;
  }

  return (
    <CardChrome
      status={status}
      neutralOnDone={!agent || agent.status === "stalled" || agent.status === "detached"}
      icon={<Sparkles className={ICON} />}
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

/** Unwrap opencode's `<task><task_result>…</task_result></task>` report; anything else is returned verbatim. */
export function extractTaskResult(text: string): string {
  const match = text.trim().match(/^<task\b[^>]*>\s*<task_result\b[^>]*>([\s\S]*?)<\/task_result>\s*<\/task>$/);
  return match ? match[1]!.trim() : text;
}

/** A sub-agent Task. With children it is Claude's linked Task; childless it is an
 *  off-protocol launch (opencode) that only reports a final result. */
export function SubagentCard({ tool, result, children }: ToolCardProps & { children: ToolCardProps[] }) {
  const { args, description } = useTaskDescription(tool);
  const hasChildren = children.length > 0;
  const prompt = pickStr(args, "prompt");
  const report = result ? extractTaskResult(result.text ?? "") : "";
  // Only the parent's own error fails the header.
  const status: Status =
    result === undefined || children.some((c) => !c.result) ? "running" : result.kind === "tool_error" ? "err" : "ok";
  const [open, setOpen] = useToolCardExpansion(status);

  return (
    <CardChrome
      status={status}
      {...spanTimes([{ tool, result }, ...children])}
      icon={<Sparkles className={ICON} />}
      label="subagent"
      primary={
        <>
          <span className="truncate">{description}</span>
          {hasChildren && (
            <span className="ml-2 text-text-dim">
              · {children.length} {children.length === 1 ? "tool" : "tools"}
            </span>
          )}
        </>
      }
      expanded={open}
      onToggle={() => setOpen((v) => !v)}
      body={
        open &&
        (hasChildren ? (
          <ChildCards items={children} nested />
        ) : (
          <ToolErrorBody status={status} errorText={result?.text}>
            {prompt && <HighlightedBlock text={prompt} language="markdown" maxLines={8} />}
            {status === "err" ? null : report ? (
              <HighlightedBlock text={report} language="markdown" maxLines={20} />
            ) : (
              <PlaceholderLine>{status === "running" ? "Running…" : "(no result)"}</PlaceholderLine>
            )}
          </ToolErrorBody>
        ))
      }
    />
  );
}
