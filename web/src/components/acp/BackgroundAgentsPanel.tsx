// The session's background work, shells and sub-agents, from the store fed by
// StructuredView's existing WebSocket, so this pane opens no connection of its own.

import { useCallback, useEffect, useMemo, useState } from "react";
import { Bot, ChevronDown, ChevronLeft, ChevronRight, Square, Workflow } from "lucide-react";

import {
  useActivity,
  useBackgroundAgents,
  useBackgroundShells,
  useBackgroundTasks,
  useSubagents,
  useWorkflowRuns,
} from "../../hooks/useAcpSession";
import type { BackgroundAgent, BackgroundAgentStatus, SubagentRecord } from "../../lib/acpTypes";
import { cancelSubagent } from "../../lib/api";
import { reportError } from "../../lib/toastBus";
import {
  BackgroundAgentsContext,
  findSubagent,
  isActiveAgent,
  isStoppable,
  paneAgents,
  type PaneAgent,
  workflowPhases,
  workflowRunning,
} from "./backgroundAgentsContext";
import { ShellDetailModal, ShellRow } from "./BackgroundShells";
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

type Kind = "all" | "shells" | "agents";

export function BackgroundAgentsPanel({ sessionId, readOnly, focus, onShowThread }: PanelProps) {
  const background = useBackgroundAgents(sessionId);
  const subagents = useSubagents(sessionId);
  const activity = useActivity(sessionId);
  const agents = useMemo(() => paneAgents(background, subagents, activity), [background, subagents, activity]);
  const stopSession = readOnly ? null : sessionId;
  const backgroundTasks = useBackgroundTasks(sessionId);
  const workflowRuns = useWorkflowRuns(sessionId);
  const shells = useBackgroundShells(sessionId);
  const [kind, setKind] = useState<Kind>("all");
  const [openShell, setOpenShell] = useState<string | null>(null);
  const closeShell = useCallback(() => setOpenShell(null), []);
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
    () => ({
      sessionId: sessionId ?? undefined,
      agents,
      subagents,
      readOnly,
      cancelSubagent: async (childId: string) => {
        const error = sessionId ? await cancelSubagent(sessionId, childId) : null;
        if (error) reportError(error);
      },
    }),
    [sessionId, agents, subagents, readOnly],
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
          subagents={readOnly ? [] : subagents}
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
  // The chips show only while both kinds exist; without them every kind shows.
  const filter = agents.length > 0 && shells.length > 0 ? kind : "all";
  const showShells = filter !== "agents";
  const showAgents = filter !== "shells";
  const live = (entry: Entry) =>
    entry.kind === "agent"
      ? isActiveAgent(entry.agent)
      : workflowRunning(workflowRuns[entry.agents[0]!.toolCallId], backgroundTasks, entry.agents);
  const all = showAgents ? entries(sorted, agents) : [];
  const shellsBy = (finished: boolean) => (showShells ? shells.filter((s) => s.finished === finished).reverse() : []);
  const groups = [
    { label: "Running", shells: shellsBy(false), entries: all.filter(live) },
    { label: "Recent", shells: shellsBy(true), entries: all.filter((e) => !live(e)) },
  ];
  const opened = shells.find((s) => s.taskId === openShell);
  const chip = (value: Kind, label: string) => (
    <button
      key={value}
      type="button"
      aria-pressed={kind === value}
      onClick={() => setKind(value)}
      className={`rounded px-1.5 py-0.5 text-[11px] ${kind === value ? "bg-surface-700 text-text-primary" : "text-text-dim hover:text-text-secondary"}`}
    >
      {label}
    </button>
  );

  return (
    <div className="flex h-full flex-col overflow-y-auto">
      <div className="flex flex-wrap items-center gap-2 border-b border-surface-700 px-3 py-1.5">
        <span className="text-[11px] uppercase tracking-wider text-text-dim">
          Background · {agents.length + shells.length}
        </span>
        <span className="flex flex-1 items-center gap-0.5" role="group" aria-label="Filter background work">
          {agents.length > 0 &&
            shells.length > 0 && [
              chip("all", "All"),
              chip("shells", `Shells ${shells.length}`),
              chip("agents", `Agents ${agents.length}`),
            ]}
        </span>
        {running > 0 && sessionId && !readOnly && <StopSession sessionId={sessionId} running={running} />}
      </div>
      {groups.map(
        (group) =>
          group.shells.length + group.entries.length > 0 && (
            <div key={group.label} className="border-b border-surface-700">
              <div className="px-3 pt-1.5 text-[10px] uppercase tracking-wider text-text-dim">
                {group.label} · {group.shells.length + group.entries.length}
              </div>
              {group.shells.map((s) => (
                <ShellRow key={s.taskId} shell={s} onOpen={setOpenShell} />
              ))}
              {group.entries.map((entry) =>
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
                    live={live(entry)}
                  />
                ),
              )}
            </div>
          ),
      )}
      {opened && sessionId && (
        <ShellDetailModal
          key={`${sessionId}:${opened.taskId}`}
          sessionId={sessionId}
          shell={opened}
          onClose={closeShell}
        />
      )}
    </div>
  );
}

/** The shared detail view for one agent, stepping through the list in its order. */
function AgentDetailView({
  agents,
  subagents,
  index,
  onSelect,
  onBack,
  onShowThread,
}: {
  agents: PaneAgent[];
  subagents: SubagentRecord[];
  index: number;
  onSelect: (id: string) => void;
  onBack: () => void;
  onShowThread?: () => void;
}) {
  const agent = agents[index]!;
  const record = useMemo(() => agent.record ?? fromBackground(agent), [agent]);
  const roster = findSubagent(subagents, agent.toolCallId);
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
          All background work
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
          stopChildId={isStoppable(roster) && isActiveAgent(agent) ? roster.session_id : undefined}
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
  stopped: "stopped",
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

/** Live-ticking while active, fixed once ended; absent for a terminal agent with no end time. */
function Elapsed({ startedAt, endedAt, active }: { startedAt: string; endedAt: string | null; active: boolean }) {
  const [now, setNow] = useState(() => Date.now());
  useEffect(() => {
    if (!active || endedAt) return;
    const id = setInterval(() => setNow(Date.now()), 1000);
    return () => clearInterval(id);
  }, [active, endedAt]);
  if (!active && !endedAt) return null;
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
