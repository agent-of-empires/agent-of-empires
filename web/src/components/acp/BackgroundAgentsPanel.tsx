// Async sub-agents of the active session, from the store fed by StructuredView's
// existing WebSocket, so this pane opens no connection of its own.

import { useEffect, useMemo, useState } from "react";
import { Bot, ChevronDown, ChevronLeft, ChevronRight, Square, Workflow } from "lucide-react";

import {
  useBackgroundAgents,
  useBackgroundShells,
  useBackgroundTasks,
  useSubagents,
  useWorkflowRuns,
} from "../../hooks/useAcpSession";
import type { BackgroundAgent, BackgroundAgentStatus, SubagentRecord } from "../../lib/acpTypes";
import { cancelSubagent } from "../../lib/api";
import {
  BackgroundAgentsContext,
  findSubagent,
  isActiveAgent,
  paneAgents,
  workflowPhases,
  workflowRunning,
} from "./backgroundAgentsContext";
import { BackgroundShells } from "./BackgroundShells";
import { fromBackground } from "./subagents/model";
import { StopSession } from "./subagents/StopSession";
import { SubagentDetail } from "./subagents/SubagentDetail";

type Entry =
  | { kind: "agent"; agent: BackgroundAgent }
  | { kind: "workflow"; runId: string; name: string; agents: BackgroundAgent[] };

/** Plain agents as they sort; a workflow's agents together, where its first one sorts. */
function entries(sorted: BackgroundAgent[], all: BackgroundAgent[]): Entry[] {
  const seen = new Set<string>();
  const out: Entry[] = [];
  for (const agent of sorted) {
    const workflow = agent.workflow;
    if (!workflow) {
      out.push({ kind: "agent", agent });
    } else if (!seen.has(workflow.run_id)) {
      seen.add(workflow.run_id);
      const agents = all.filter((a) => a.workflow?.run_id === workflow.run_id);
      out.push({ kind: "workflow", runId: workflow.run_id, name: workflow.name, agents });
    }
  }
  return out;
}

export interface AgentFocus {
  agentId: string;
  /** Distinguishes repeat requests for the same agent. */
  nonce: number;
}

interface PanelProps {
  sessionId: string | null;
  /** Cancel is unavailable on a read-only server. */
  readOnly?: boolean;
  /** An agent to open on; set when an inline card asks for its detail. */
  focus?: AgentFocus | null;
  /** Reveals the transcript where the pane covers it, as on mobile. */
  onShowThread?: () => void;
}

export function BackgroundAgentsPanel({ sessionId, readOnly, focus, onShowThread }: PanelProps) {
  const background = useBackgroundAgents(sessionId);
  const subagents = useSubagents(sessionId);
  const agents = useMemo(() => paneAgents(background, subagents), [background, subagents]);
  const stopSession = readOnly ? null : sessionId;
  const backgroundTasks = useBackgroundTasks(sessionId);
  const workflowRuns = useWorkflowRuns(sessionId);
  const shells = useBackgroundShells(sessionId);
  const [selected, setSelected] = useState<string | null>(focus?.agentId ?? null);
  const [seenFocus, setSeenFocus] = useState(focus);
  if (focus !== seenFocus) {
    setSeenFocus(focus);
    if (focus) setSelected(focus.agentId);
  }

  // Running first, then most-recently-started within each group.
  const sorted = useMemo(
    () =>
      [...agents].sort((a, b) => {
        const ra = isActiveAgent(a) ? 0 : 1;
        const rb = isActiveAgent(b) ? 0 : 1;
        if (ra !== rb) return ra - rb;
        return b.startedAt.localeCompare(a.startedAt);
      }),
    [agents],
  );
  const context = useMemo(
    () => ({ sessionId: sessionId ?? undefined, agents, readOnly }),
    [sessionId, agents, readOnly],
  );

  if (agents.length === 0 && shells.length === 0) {
    return (
      <div className="flex h-full items-center justify-center px-4 text-center text-xs text-text-dim">
        No background work yet. Async <span className="font-mono">Task</span> sub-agents,{" "}
        <span className="font-mono">Workflow</span> runs and background shell commands show up here.
      </div>
    );
  }

  const index = sorted.findIndex((a) => a.agentId === selected);
  if (index >= 0) {
    return (
      <BackgroundAgentsContext.Provider value={context}>
        <AgentDetailView
          agents={sorted}
          index={index}
          onSelect={setSelected}
          onBack={() => setSelected(null)}
          onShowThread={onShowThread}
        />
      </BackgroundAgentsContext.Provider>
    );
  }

  // A stalled agent is no longer writing, so cancel would be a no-op.
  const running = agents.filter((a) => a.status === "running").length;

  // One stable slot for the shells: moving it remounts the shell dialog and closes it.
  const hasAgents = agents.length > 0;
  return (
    <div className="flex h-full flex-col overflow-y-auto">
      {hasAgents && (
        <div className="flex flex-wrap items-center gap-2 border-b border-surface-700 px-3 py-1.5">
          <span className="flex-1 text-[11px] uppercase tracking-wider text-text-dim">
            Background agents · {agents.length}
          </span>
          {running > 0 && sessionId && !readOnly && <StopSession sessionId={sessionId} running={running} />}
        </div>
      )}
      {sessionId && <BackgroundShells sessionId={sessionId} />}
      {hasAgents && (
        <>
          <p className="border-b border-surface-800 px-3 py-1 text-[11px] leading-snug text-text-dim">
            Delegated sub-agents, async (background) <span className="font-mono">Task</span> sub-agents and{" "}
            <span className="font-mono">Workflow</span> agents show here. Inline sub-agents also appear in the
            transcript.
          </p>
          <div className="flex flex-col">
            {entries(sorted, agents).map((entry) =>
              entry.kind === "agent" ? (
                <AgentRow
                  key={entry.agent.agentId}
                  agent={entry.agent}
                  onSelect={setSelected}
                  sessionId={stopSession}
                  subagents={subagents}
                />
              ) : (
                <WorkflowGroup
                  key={entry.runId}
                  name={entry.name}
                  agents={entry.agents}
                  onSelect={setSelected}
                  sessionId={stopSession}
                  subagents={subagents}
                  live={workflowRunning(workflowRuns[entry.agents[0]!.toolCallId], backgroundTasks, entry.agents)}
                />
              ),
            )}
          </div>
        </>
      )}
    </div>
  );
}

/** The shared detail view for one agent, stepping through the list in its order. */
function AgentDetailView({
  agents,
  index,
  onSelect,
  onBack,
  onShowThread,
}: {
  agents: BackgroundAgent[];
  index: number;
  onSelect: (id: string) => void;
  onBack: () => void;
  onShowThread?: () => void;
}) {
  const agent = agents[index]!;
  const record = useMemo(() => fromBackground(agent), [agent]);
  const inThread = useMemo(
    () => document.querySelector(`[data-tool-id="${CSS.escape(agent.toolCallId)}"]`) !== null,
    [agent.toolCallId],
  );
  const step = (by: number) => onSelect(agents[index + by]!.agentId);
  const NAV = "rounded p-1 text-text-dim hover:text-text-secondary disabled:opacity-30";
  return (
    <div className="flex h-full flex-col overflow-y-auto">
      <div className="flex items-center gap-2 border-b border-surface-700 px-2 py-1.5 text-[11px] text-text-dim">
        <button type="button" onClick={onBack} className="inline-flex items-center gap-0.5 hover:text-text-secondary">
          <ChevronLeft className="h-3.5 w-3.5" />
          All agents
        </button>
        <span className="flex-1 text-center">
          Subagent {index + 1} of {agents.length}
        </span>
        <button
          type="button"
          aria-label="Previous subagent"
          disabled={index === 0}
          onClick={() => step(-1)}
          className={NAV}
        >
          <ChevronLeft className="h-4 w-4" />
        </button>
        <button
          type="button"
          aria-label="Next subagent"
          disabled={index === agents.length - 1}
          onClick={() => step(1)}
          className={NAV}
        >
          <ChevronRight className="h-4 w-4" />
        </button>
      </div>
      <div className="px-2">
        <SubagentDetail
          key={agent.agentId}
          record={record}
          alwaysOpen
          actions={
            inThread && (
              <button
                type="button"
                onClick={() => {
                  onShowThread?.();
                  requestAnimationFrame(() =>
                    document
                      .querySelector(`[data-tool-id="${CSS.escape(agent.toolCallId)}"]`)
                      ?.scrollIntoView({ block: "center" }),
                  );
                }}
                className="text-[11px] text-text-dim hover:text-text-secondary"
              >
                Show in thread
              </button>
            )
          }
        />
      </div>
    </div>
  );
}

function WorkflowGroup({
  name,
  agents,
  live,
  onSelect,
  sessionId,
  subagents,
}: {
  name: string;
  agents: BackgroundAgent[];
  live: boolean;
  onSelect: (id: string) => void;
  sessionId: string | null;
  subagents: SubagentRecord[];
}) {
  const [open, setOpen] = useState(true);
  const done = agents.filter((a) => !isActiveAgent(a)).length;
  return (
    <div className="border-b border-surface-700">
      <button
        type="button"
        onClick={() => setOpen((v) => !v)}
        className="flex w-full items-center gap-2 px-3 py-2 text-left hover:bg-surface-800"
      >
        <StatusDot status={live ? "running" : "completed"} />
        <Workflow className="h-3.5 w-3.5 shrink-0 text-text-dim" />
        <span className="min-w-0 flex-1 truncate text-xs text-text-secondary">{name}</span>
        <span className="shrink-0 text-[11px] tabular-nums text-text-dim">
          {done} of {agents.length} done
        </span>
        <ChevronDown
          className={`h-3.5 w-3.5 shrink-0 text-text-dim transition-transform ${open ? "rotate-180" : ""}`}
        />
      </button>
      {open &&
        workflowPhases(agents).map(({ phase, agents: phaseAgents }) => (
          <div key={phase} className="pl-3">
            {phase && (
              <div className="px-3 pt-1 text-[10px] uppercase tracking-wider text-text-dim">
                {phase} · {phaseAgents.filter((a) => !isActiveAgent(a)).length}/{phaseAgents.length}
              </div>
            )}
            {phaseAgents.map((a) => (
              <AgentRow key={a.agentId} agent={a} onSelect={onSelect} sessionId={sessionId} subagents={subagents} />
            ))}
          </div>
        ))}
    </div>
  );
}

function AgentRow({
  agent,
  onSelect,
  sessionId,
  subagents,
}: {
  agent: BackgroundAgent;
  onSelect: (id: string) => void;
  sessionId: string | null;
  subagents: SubagentRecord[];
}) {
  const [stopError, setStopError] = useState<string | null>(null);
  const roster = findSubagent(subagents, agent.toolCallId);
  const stoppable = !!roster?.cancel_capable && !!sessionId && isActiveAgent(agent);
  return (
    <div className="border-b border-surface-800">
      <div className="flex items-center hover:bg-surface-800">
        <button
          type="button"
          onClick={() => onSelect(agent.agentId)}
          className="flex min-w-0 flex-1 items-center gap-2 px-3 py-2 text-left"
        >
          <StatusDot status={agent.status} />
          <Bot className="h-3.5 w-3.5 shrink-0 text-text-dim" />
          <span className="min-w-0 flex-1 truncate text-xs text-text-secondary">
            {agent.description || "Sub-agent"}
          </span>
          <Elapsed startedAt={agent.startedAt} endedAt={agent.endedAt} active={isActiveAgent(agent)} />
          <StatusLabel status={agent.status} toolCount={agent.toolCount} />
          <ChevronRight className="h-3.5 w-3.5 shrink-0 text-text-dim" />
        </button>
        {stoppable && (
          <button
            type="button"
            title="Stop this sub-agent"
            aria-label="Stop sub-agent"
            onClick={async () => setStopError(await cancelSubagent(sessionId, roster.session_id))}
            className="shrink-0 px-1.5 py-2 text-text-dim hover:text-rose-300"
          >
            <Square className="h-3 w-3 fill-current" strokeWidth={0} />
          </button>
        )}
      </div>
      {stopError && <div className="px-3 pb-1.5 pl-9 text-[11px] text-status-error">{stopError}</div>}
      {agent.lastText && <div className="truncate px-3 pb-1.5 pl-9 text-[11px] text-text-dim">{agent.lastText}</div>}
    </div>
  );
}

function StatusDot({ status }: { status: BackgroundAgentStatus }) {
  const cls =
    status === "running"
      ? "bg-status-waiting animate-pulse"
      : status === "requires_action"
        ? "bg-status-waiting"
        : status === "completed"
          ? "bg-status-running"
          : status === "error"
            ? "bg-status-error"
            : "bg-text-dim/60"; // stalled / detached / unknown
  return <span className={`h-2 w-2 shrink-0 rounded-full ${cls}`} />;
}

const STATUS_LABELS: Record<Exclude<BackgroundAgentStatus, "running">, string> = {
  completed: "done",
  stalled: "stalled",
  detached: "detached",
  error: "error",
  requires_action: "Waiting on you",
  unknown: "Unknown",
};

function StatusLabel({ status, toolCount }: { status: BackgroundAgentStatus; toolCount: number }) {
  if (status === "running") {
    return (
      <span className="shrink-0 text-[11px] text-text-dim">
        running{toolCount > 0 ? ` · ${toolCount} ${toolCount === 1 ? "tool" : "tools"}` : ""}
      </span>
    );
  }
  const label = STATUS_LABELS[status];
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
