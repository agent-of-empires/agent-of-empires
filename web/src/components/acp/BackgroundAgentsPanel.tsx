// Background agents panel.
//
// Lists the async sub-agents (Claude `Task` with isAsync) launched in the
// active structured-view session, with live status, elapsed time, current
// activity, and (on completion) the result. Data comes from the
// `useBackgroundAgents` store, which reuses the single ACP WebSocket
// subscription <StructuredView> already holds, so this sibling pane does
// not open a second connection. See src/acp/background_agent.rs for the
// backend tailer that produces the events.

import { useEffect, useState } from "react";
import { Bot, ChevronDown } from "lucide-react";

import { useBackgroundAgents } from "../../hooks/useAcpSession";
import type { BackgroundAgent, BackgroundAgentStatus } from "../../lib/acpTypes";

export function BackgroundAgentsPanel({ sessionId }: { sessionId: string | null }) {
  const agents = useBackgroundAgents(sessionId);

  if (agents.length === 0) {
    return (
      <div className="flex h-full items-center justify-center px-4 text-center text-xs text-text-dim">
        No background sub-agents launched yet. When the agent dispatches an async{" "}
        <span className="font-mono">Task</span>, it shows up here with live progress.
      </div>
    );
  }

  // Running first, then most-recently-started within each group.
  const sorted = [...agents].sort((a, b) => {
    const ra = isActive(a.status) ? 0 : 1;
    const rb = isActive(b.status) ? 0 : 1;
    if (ra !== rb) return ra - rb;
    return b.startedAt.localeCompare(a.startedAt);
  });

  return (
    <div className="flex h-full flex-col overflow-y-auto">
      <div className="border-b border-surface-700 px-3 py-1.5 text-[11px] uppercase tracking-wider text-text-dim">
        Sub agents · {agents.length}
      </div>
      <div className="flex flex-col">
        {sorted.map((a) => (
          <AgentRow key={a.agentId} agent={a} />
        ))}
      </div>
    </div>
  );
}

function isActive(status: BackgroundAgentStatus): boolean {
  return status === "running" || status === "stalled";
}

function AgentRow({ agent }: { agent: BackgroundAgent }) {
  const [open, setOpen] = useState(false);
  return (
    <div className="border-b border-surface-800">
      <button
        type="button"
        onClick={() => setOpen((v) => !v)}
        className="flex w-full items-center gap-2 px-3 py-2 text-left hover:bg-surface-800"
      >
        <StatusDot status={agent.status} />
        <Bot className="h-3.5 w-3.5 shrink-0 text-text-dim" />
        <span className="min-w-0 flex-1 truncate text-xs text-text-secondary">{agent.description || "Sub-agent"}</span>
        <Elapsed startedAt={agent.startedAt} endedAt={agent.endedAt} active={isActive(agent.status)} />
        <StatusLabel status={agent.status} toolCount={agent.toolCount} />
        <ChevronDown
          className={`h-3.5 w-3.5 shrink-0 text-text-dim transition-transform ${open ? "rotate-180" : ""}`}
        />
      </button>
      {!open && agent.lastText && (
        <div className="truncate px-3 pb-1.5 pl-9 text-[11px] text-text-dim">{agent.lastText}</div>
      )}
      {open && (
        <div className="space-y-2 border-t border-surface-800 bg-surface-900/30 px-3 py-2 pl-9 text-[11px]">
          {agent.warning && <Field label="warning" value={agent.warning} tone="warn" />}
          <Field label="model" value={agent.model || "unknown"} mono />
          {agent.lastTool && <Field label="last tool" value={agent.lastTool} mono />}
          <Field label="prompt" value={agent.prompt || "(none)"} />
          {agent.result && <Field label="result" value={agent.result} />}
        </div>
      )}
    </div>
  );
}

function Field({ label, value, mono, tone }: { label: string; value: string; mono?: boolean; tone?: "warn" }) {
  return (
    <div className="flex flex-col gap-0.5">
      <span className="text-[10px] uppercase tracking-wider text-text-dim">{label}</span>
      <span
        className={[
          "whitespace-pre-wrap break-words",
          mono ? "font-mono" : "",
          tone === "warn" ? "text-status-error" : "text-text-secondary",
        ].join(" ")}
      >
        {value}
      </span>
    </div>
  );
}

function StatusDot({ status }: { status: BackgroundAgentStatus }) {
  const cls =
    status === "running"
      ? "bg-brand-400 animate-pulse"
      : status === "completed"
        ? "bg-status-running"
        : status === "error"
          ? "bg-status-error"
          : "bg-text-dim/60"; // stalled / detached
  return <span className={`h-2 w-2 shrink-0 rounded-full ${cls}`} />;
}

function StatusLabel({ status, toolCount }: { status: BackgroundAgentStatus; toolCount: number }) {
  if (status === "running") {
    return (
      <span className="shrink-0 text-[11px] text-text-dim">
        running{toolCount > 0 ? ` · ${toolCount} ${toolCount === 1 ? "tool" : "tools"}` : ""}
      </span>
    );
  }
  const label =
    status === "completed" ? "done" : status === "stalled" ? "stalled" : status === "detached" ? "detached" : "error";
  const tone = status === "error" ? "text-status-error" : "text-text-dim";
  return <span className={`shrink-0 text-[11px] ${tone}`}>{label}</span>;
}

/** Live-ticking elapsed for running agents; fixed duration once ended. */
function Elapsed({ startedAt, endedAt, active }: { startedAt: string; endedAt: string | null; active: boolean }) {
  const [now, setNow] = useState(() => Date.now());
  useEffect(() => {
    if (!active || endedAt) return;
    const id = setInterval(() => setNow(Date.now()), 1000);
    return () => clearInterval(id);
  }, [active, endedAt]);
  const start = Date.parse(startedAt);
  if (!Number.isFinite(start)) return null;
  const end = endedAt ? Date.parse(endedAt) : now;
  if (!Number.isFinite(end)) return null;
  return (
    <span className="shrink-0 text-[11px] tabular-nums text-text-dim">{formatElapsed(Math.max(0, end - start))}</span>
  );
}

function formatElapsed(ms: number): string {
  const s = Math.floor(ms / 1000);
  if (s < 60) return `${s}s`;
  const m = Math.floor(s / 60);
  if (m < 60) return `${m}m ${s % 60}s`;
  const h = Math.floor(m / 60);
  return `${h}h ${m % 60}m`;
}
