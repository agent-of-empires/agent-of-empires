/* eslint-disable react-refresh/only-export-components */
// Cards that fold several tool calls: action runs and sub-agent tasks.

import { useMemo, useState } from "react";
import { Layers } from "lucide-react";

import type { ToolCall } from "../../lib/acpTypes";
import { useBackgroundAgentFor, useOpenBackgroundAgentsPane } from "./backgroundAgentsContext";
import { CardChrome, spanTimes, useToolCardExpansion, type Status, type ToolCardProps } from "./ToolCardChrome";
import { useToolDisplayMode } from "./ToolDisplayMode";
import { useToolExpansionStore } from "./ToolExpansion";
import { ToolCard } from "./ToolCards";
import { ToolOutputMedia } from "./ToolOutputMedia";
import { fromBackground, fromInline, taskPrompt } from "./subagents/model";
import { SubagentDetail } from "./subagents/SubagentDetail";

export { extractTaskResult } from "./subagents/model";

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

/** Async sub-agent launch, linked to its live Background agents record by
 *  tool-call id. Running until the first tailer event arrives. */
export function AsyncSubagentCard({ tool }: { tool: ToolCall }) {
  const agent = useBackgroundAgentFor(tool.id);
  const openPane = useOpenBackgroundAgentsPane();
  const record = useMemo(
    () =>
      agent
        ? fromBackground(agent)
        : {
            ...fromInline(tool, undefined, []),
            background: true,
            prompt: taskPrompt(tool.args_preview),
          },
    [agent, tool],
  );
  return (
    <SubagentDetail
      record={record}
      actions={
        openPane && (
          <button
            type="button"
            onClick={() => openPane(agent?.agentId)}
            className="text-[11px] text-text-dim hover:text-text-secondary"
          >
            Open in side pane
          </button>
        )
      }
    />
  );
}

/** A sub-agent Task. With children it is Claude's linked Task; childless it is an
 *  off-protocol launch (opencode) that only reports a final result. */
export function SubagentCard({ tool, result, children }: ToolCardProps & { children: ToolCardProps[] }) {
  const record = useMemo(() => fromInline(tool, result, children), [tool, result, children]);
  return (
    <>
      <SubagentDetail record={record} />
      {result?.output?.length ? <ToolOutputMedia blocks={result.output} /> : null}
    </>
  );
}
