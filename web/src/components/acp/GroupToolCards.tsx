/* eslint-disable react-refresh/only-export-components */
// Cards that fold several tool calls: action runs and sub-agent tasks.

import { useState } from "react";
import { Layers, Sparkles, Square } from "lucide-react";

import { pickStr } from "../../lib/acpArgs";
import type { BackgroundAgentStatus, SubagentRecord, ToolCall } from "../../lib/acpTypes";
import {
  isActiveAgent,
  isStoppable,
  useBackgroundAgentFor,
  useCancelSubagent,
  useOpenBackgroundAgentsPane,
  useSubagentFor,
} from "./backgroundAgentsContext";
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
import { useToolDisplayMode } from "./ToolDisplayMode";
import { useToolExpansionStore } from "./ToolExpansion";
import { ToolCard } from "./ToolCards";
import { ToolErrorBody } from "./ToolErrorBody";
import { ToolOutputMedia } from "./ToolOutputMedia";

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
  // A run folds under the reader: start open if any of its cards showed open.
  // Failed cards open by default, so a group holding one does too in every
  // density. The seed is closed only when the reader closed a failed card, and
  // unset otherwise so a child failing later still opens the group.
  const store = useToolExpansionStore();
  const density = useToolDisplayMode();
  const [seed] = useState<boolean | undefined>(() => {
    const failed = (i: (typeof items)[number]) => i.result?.kind === "tool_error";
    if (items.some((i) => store?.isOpen(i.tool.id, density, failed(i)))) return true;
    const closedFailed = items.some((i) => failed(i) && store?.get(i.tool.id)?.density === density);
    return closedFailed ? false : undefined;
  });
  const [open, setOpen] = useToolCardExpansion(errorCount > 0 ? "err" : status, false, seed);
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
  requires_action: ["running", "Waiting on you"],
  unknown: ["stopped", "Unknown"],
};

function subagentBadge(record: SubagentRecord): string {
  switch (record.state) {
    case "running":
      return "running";
    case "requires_action":
      return "waiting on you";
    case "idle":
      return record.stop_reason ? `idle · ${record.stop_reason}` : "idle";
    default:
      return "unknown";
  }
}

/** Stops one child. The header is a button, so a nested one would be invalid markup. */
function StopSubagentButton({ sessionId }: { sessionId: string }) {
  const cancel = useCancelSubagent();
  if (!cancel) return null;
  return (
    <span
      role="button"
      tabIndex={0}
      title="Stop this sub-agent"
      aria-label="Stop sub-agent"
      onClick={(e) => {
        e.stopPropagation();
        void cancel(sessionId);
      }}
      onKeyDown={(e) => {
        if (e.key !== "Enter" && e.key !== " ") return;
        e.preventDefault();
        e.stopPropagation();
        void cancel(sessionId);
      }}
      className="ml-2 inline-flex items-center text-text-dim hover:text-rose-300"
    >
      <Square className="h-3 w-3 fill-current" strokeWidth={0} />
    </span>
  );
}

/** Async sub-agent launch, linked to its live Background agents record by
 *  tool-call id. Neutral until the first tailer event arrives. */
export function AsyncSubagentCard({ tool }: { tool: ToolCall }) {
  const { description } = useTaskDescription(tool);
  const agent = useBackgroundAgentFor(tool.id);
  const record = useSubagentFor(tool.id);
  const openPane = useOpenBackgroundAgentsPane();
  const stoppable = !!agent && isActiveAgent(agent) && isStoppable(record);

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
      meta={
        <>
          <span className="text-[11px] text-text-dim">{metaText}</span>
          {stoppable && <StopSubagentButton sessionId={record.session_id} />}
        </>
      }
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

/** A sub-agent Task or delegation. With children it nests their calls; childless it
 *  only reports a final result (opencode, or a delegation with no calls yet). */
export function SubagentCard({ tool, result: toolResult, children }: ToolCardProps & { children: ToolCardProps[] }) {
  const { args, description: toolDescription } = useTaskDescription(tool);
  const record = useSubagentFor(tool.id);
  // A background child outlives the turn that stopped its launch call, so the
  // roster, not that stop, says whether the card is still active.
  const result = record && record.state !== "unknown" && toolResult?.kind === "tool_stopped" ? undefined : toolResult;
  const description = record?.title || toolDescription;
  const hasChildren = children.length > 0;
  const prompt = pickStr(args, "prompt");
  const report = result ? extractTaskResult(result.text ?? "") : "";
  // Only the parent's own error fails the header. An idle child ends the card
  // even when no reply ever completed it.
  const idle = record?.state === "idle";
  const pending = result === undefined || children.some((c) => !c.result);
  const failed = result ? result.kind === "tool_error" : record?.stop_reason === "error";
  const status: Status = pending && !idle ? "running" : failed ? "err" : "ok";
  const [open, setOpen] = useToolCardExpansion(status);

  return (
    <>
      <CardChrome
        status={status}
        // The roster has no end time, so a child settled without a reply shows no duration.
        {...(idle && !result ? {} : spanTimes([{ tool, result }, ...children]))}
        icon={<Sparkles className={ICON} />}
        label="subagent"
        meta={
          record && (
            <>
              <span className="text-[11px] text-text-dim">{subagentBadge(record)}</span>
              {isStoppable(record) && <StopSubagentButton sessionId={record.session_id} />}
            </>
          )
        }
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
      {result?.output?.length ? <ToolOutputMedia blocks={result.output} /> : null}
    </>
  );
}
