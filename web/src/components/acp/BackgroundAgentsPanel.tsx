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
import { Bot, ChevronDown, Maximize2, Square, X } from "lucide-react";

import { useBackgroundAgents } from "../../hooks/useAcpSession";
import type { BackgroundAgent, BackgroundAgentStatus, BackgroundAgentTool } from "../../lib/acpTypes";

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
  // Stop only when something is genuinely running; a stalled agent has
  // stopped writing, so /acp/cancel would be a confusing no-op.
  const anyRunning = agents.some((a) => a.status === "running");

  return (
    <div className="flex h-full flex-col overflow-y-auto">
      <div className="flex items-center gap-2 border-b border-surface-700 px-3 py-1.5">
        <span className="flex-1 text-[11px] uppercase tracking-wider text-text-dim">
          Background agents · {agents.length}
        </span>
        {anyRunning && sessionId && <StopButton sessionId={sessionId} />}
      </div>
      <p className="border-b border-surface-800 px-3 py-1 text-[11px] leading-snug text-text-dim">
        Only async (background) <span className="font-mono">Task</span> sub-agents show here. Synchronous sub-agents run
        inline in the transcript.
      </p>
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

/** Interrupt the session, which stops the SDK's in-flight async sub-agents.
 *  ACP has no per-agent cancel, so this is the same `/acp/cancel` the
 *  composer Stop uses, reachable here because the panel sits in a sibling
 *  dock with no composer turn of its own. */
function StopButton({ sessionId }: { sessionId: string }) {
  const [busy, setBusy] = useState(false);
  return (
    <button
      type="button"
      disabled={busy}
      title="Interrupt the session and stop running sub-agents"
      onClick={async () => {
        setBusy(true);
        try {
          await fetch(`/api/sessions/${encodeURIComponent(sessionId)}/acp/cancel`, { method: "POST" });
        } catch {
          // best-effort; the tailer marks idle agents stalled regardless
        } finally {
          setBusy(false);
        }
      }}
      className={[
        "inline-flex items-center gap-1 rounded-md border border-surface-600 bg-surface-800 px-2 py-0.5",
        "text-[11px] text-text-secondary transition-colors",
        "hover:border-rose-700/60 hover:bg-rose-950/30 hover:text-rose-300",
        busy ? "opacity-50" : "",
      ].join(" ")}
    >
      <Square className="h-3 w-3 fill-current" strokeWidth={0} />
      Stop
    </button>
  );
}

function AgentRow({ agent }: { agent: BackgroundAgent }) {
  const [open, setOpen] = useState(false);
  const [modal, setModal] = useState(false);
  return (
    <div className="border-b border-surface-800">
      <div className="flex items-center hover:bg-surface-800">
        <button
          type="button"
          onClick={() => setOpen((v) => !v)}
          className="flex min-w-0 flex-1 items-center gap-2 px-3 py-2 text-left"
        >
          <StatusDot status={agent.status} />
          <Bot className="h-3.5 w-3.5 shrink-0 text-text-dim" />
          <span className="min-w-0 flex-1 truncate text-xs text-text-secondary">
            {agent.description || "Sub-agent"}
          </span>
          <Elapsed startedAt={agent.startedAt} endedAt={agent.endedAt} active={isActive(agent.status)} />
          <StatusLabel status={agent.status} toolCount={agent.toolCount} />
          <ChevronDown
            className={`h-3.5 w-3.5 shrink-0 text-text-dim transition-transform ${open ? "rotate-180" : ""}`}
          />
        </button>
        <button
          type="button"
          onClick={() => setModal(true)}
          title="Open full details"
          aria-label="Open full details"
          className="shrink-0 px-2 py-2 text-text-dim hover:text-text-secondary"
        >
          <Maximize2 className="h-3.5 w-3.5" />
        </button>
      </div>
      {!open && agent.lastText && (
        <div className="truncate px-3 pb-1.5 pl-9 text-[11px] text-text-dim">{agent.lastText}</div>
      )}
      {open && (
        <div className="space-y-2 border-t border-surface-800 bg-surface-900/30 px-3 py-2 pl-9 text-[11px]">
          {agent.warning && <Field label="warning" value={agent.warning} tone="warn" />}
          <Field label="model" value={agent.model || "unknown"} mono />
          {agent.tools.length > 0 && <ToolList tools={agent.tools} />}
          <Field label="prompt" value={agent.prompt || "(none)"} clamp />
          {agent.result && <Field label="result" value={agent.result} clamp />}
        </div>
      )}
      {modal && <AgentDetailModal agent={agent} onClose={() => setModal(false)} />}
    </div>
  );
}

/** Full-detail modal for one sub-agent: the prompt, result, and every
 *  tool call shown in full (the panel is narrow and clamps long text). */
function AgentDetailModal({ agent, onClose }: { agent: BackgroundAgent; onClose: () => void }) {
  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      if (e.key === "Escape") onClose();
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [onClose]);
  return (
    <div
      className="fixed inset-0 z-50 flex items-center justify-center bg-black/60 animate-fade-in"
      onClick={onClose}
      role="dialog"
      aria-modal="true"
    >
      <div
        className="flex max-h-[85vh] w-[680px] max-w-[92vw] flex-col rounded-lg border border-surface-700/50 bg-surface-800 shadow-2xl"
        onClick={(e) => e.stopPropagation()}
      >
        <div className="flex items-center gap-2 border-b border-surface-700 px-4 py-3">
          <Bot className="h-4 w-4 shrink-0 text-text-dim" />
          <span className="min-w-0 flex-1 truncate text-sm font-semibold text-text-bright">
            {agent.description || "Sub-agent"}
          </span>
          <StatusLabel status={agent.status} toolCount={agent.toolCount} />
          <button
            type="button"
            onClick={onClose}
            aria-label="Close"
            className="text-text-muted hover:text-text-secondary"
          >
            <X className="h-4 w-4" />
          </button>
        </div>
        <div className="space-y-3 overflow-y-auto px-4 py-3 text-[12px]">
          {agent.warning && <Field label="warning" value={agent.warning} tone="warn" />}
          <Field label="model" value={agent.model || "unknown"} mono />
          {agent.tools.length > 0 && <ToolList tools={agent.tools} />}
          <Field label="prompt" value={agent.prompt || "(none)"} />
          {agent.result && <Field label="result" value={agent.result} />}
        </div>
      </div>
    </div>
  );
}

/** The sub-agent's individual tool calls, like the main output: one row
 *  per read / bash / grep with its target and outcome. */
function ToolList({ tools }: { tools: BackgroundAgentTool[] }) {
  return (
    <div className="flex flex-col gap-0.5">
      <span className="text-[10px] uppercase tracking-wider text-text-dim">tools · {tools.length}</span>
      <div className="flex flex-col gap-0.5">
        {tools.map((t, i) => (
          <div key={i} className="flex items-start gap-1.5" title={t.title ? `${t.name} ${t.title}` : t.name}>
            <span className="mt-1">
              <ToolDot ok={t.ok} />
            </span>
            <span className="shrink-0 font-mono text-text-secondary">{t.name}</span>
            {t.title && <span className="min-w-0 break-all font-mono text-text-dim">{t.title}</span>}
          </div>
        ))}
      </div>
    </div>
  );
}

function ToolDot({ ok }: { ok?: boolean | null }) {
  const cls =
    ok === undefined || ok === null ? "bg-status-waiting animate-pulse" : ok ? "bg-status-running" : "bg-status-error";
  return <span className={`h-1.5 w-1.5 shrink-0 rounded-full ${cls}`} />;
}

function Field({
  label,
  value,
  mono,
  tone,
  clamp,
}: {
  label: string;
  value: string;
  mono?: boolean;
  tone?: "warn";
  /** In the narrow inline row, cap long text to a few lines; the details
   *  modal renders without this so prompt/result show in full. */
  clamp?: boolean;
}) {
  return (
    <div className="flex flex-col gap-0.5">
      <span className="text-[10px] uppercase tracking-wider text-text-dim">{label}</span>
      <span
        className={[
          "whitespace-pre-wrap break-words",
          clamp ? "line-clamp-4" : "",
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
      ? "bg-status-waiting animate-pulse"
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
