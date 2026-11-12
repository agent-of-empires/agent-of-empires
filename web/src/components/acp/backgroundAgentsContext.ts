// Background agents and the pane opener, so an inline async Task or Workflow card can show its live entries.

import { createContext, useContext, useMemo } from "react";

import {
  subagentStatus,
  type ActivityRow,
  type BackgroundAgent,
  type BackgroundAgentStatus,
  type SubagentRecord,
  type WorkflowRun,
} from "../../lib/acpTypes";
import {
  fromInline,
  launchFromRows,
  type SubagentRecord as SubagentView,
  type SubagentStatus,
} from "./subagents/model";

export interface BackgroundAgentsContextValue {
  sessionId?: string;
  agents: BackgroundAgent[];
  subagents?: SubagentRecord[];
  /** Stops one subagent; failures surface as a session error. */
  cancelSubagent?: (childId: string) => Promise<void>;
  workflowRuns?: Record<string, WorkflowRun>;
  backgroundTasks?: string[];
  /** Opens the Background pane, on one agent's detail when given its id. */
  openPane?: (agentId?: string) => void;
  /** Cancel is unavailable: a read-only server or a trashed/archived session. */
  readOnly?: boolean;
}

export const BackgroundAgentsContext = createContext<BackgroundAgentsContextValue>({ agents: [] });

export function useBackgroundAgentsContextValue(): BackgroundAgentsContextValue {
  return useContext(BackgroundAgentsContext);
}

export function useBackgroundAgentFor(toolCallId: string | undefined): BackgroundAgent | undefined {
  const { agents } = useContext(BackgroundAgentsContext);
  if (!toolCallId) return undefined;
  return agents.find((a) => a.toolCallId === toolCallId);
}

/** What a card or pane row links a child by: its launch card, else its own session. */
export const subagentKey = (s: SubagentRecord): string => s.parent_tool_call_id ?? s.session_id;

/** The roster record for the subagent a card or pane row stands for. */
export function findSubagent(
  subagents: SubagentRecord[] | undefined,
  toolCallId: string | undefined,
): SubagentRecord | undefined {
  if (!toolCallId) return undefined;
  return subagents?.find((s) => subagentKey(s) === toolCallId);
}

/** A running child is stoppable when the adapter accepts a cancel for it. */
export const isStoppable = (s: SubagentRecord | undefined): s is SubagentRecord =>
  !!s?.cancel_capable && s.state !== "idle";

export type PaneAgent = BackgroundAgent & { record?: SubagentView };

const AGENT_STATUS: Record<SubagentStatus, BackgroundAgentStatus> = {
  running: "running",
  waiting: "requires_action",
  done: "completed",
  stopped: "stopped",
  failed: "error",
  stalled: "stalled",
  detached: "detached",
  unknown: "unknown",
};

/** The pane's rows: legacy launch records, plus every child no record covers, built
 *  from the same activity rows as its thread card. Before that card's row loads,
 *  a child shows only its roster title and state. */
export function paneAgents(
  agents: BackgroundAgent[],
  subagents: SubagentRecord[],
  rows: readonly ActivityRow[],
): PaneAgent[] {
  const linked = new Set(agents.map((a) => a.toolCallId));
  const children = subagents
    .filter((s) => !linked.has(subagentKey(s)))
    .map((s): PaneAgent => {
      const key = subagentKey(s);
      const launch = launchFromRows(rows, key);
      const record = launch && fromInline(launch.tool, launch.result, launch.children, s);
      return {
        agentId: s.session_id,
        toolCallId: key,
        description: record?.description ?? (s.title || s.description || ""),
        prompt: record?.prompt ?? "",
        model: s.model ?? "",
        status: record ? AGENT_STATUS[record.status] : subagentStatus(s),
        startedAt: record?.startedAt ?? "",
        endedAt: record?.endedAt ?? null,
        toolCount: record?.toolCount ?? 0,
        tools: [],
        lastTool: record?.lastTool ?? null,
        lastText: null,
        result: record?.report || null,
        warning: s.error ?? null,
        record,
      };
    });
  return [...agents, ...children];
}

export function useSubagentFor(toolCallId: string | undefined): SubagentRecord | undefined {
  return findSubagent(useContext(BackgroundAgentsContext).subagents, toolCallId);
}

/** Every agent a `Workflow` call launched, in launch order. */
export function useBackgroundAgentsFor(toolCallId: string): BackgroundAgent[] {
  const { agents } = useContext(BackgroundAgentsContext);
  return useMemo(() => agents.filter((a) => a.toolCallId === toolCallId), [agents, toolCallId]);
}

export function useWorkflowRun(toolCallId: string): WorkflowRun | undefined {
  return useContext(BackgroundAgentsContext).workflowRuns?.[toolCallId];
}

export function useBackgroundTaskIds(): string[] {
  return useContext(BackgroundAgentsContext).backgroundTasks ?? NO_TASKS;
}

const NO_TASKS: string[] = [];

/** A run is live while its background task is; its agents only fill the phase
 *  counts. A run launched before the replay window has no record, so its
 *  agents decide. */
export function workflowRunning(
  run: WorkflowRun | undefined,
  backgroundTasks: string[],
  agents: BackgroundAgent[],
): boolean {
  return run ? !run.finished && backgroundTasks.includes(run.taskId) : agents.some(isActiveAgent);
}

export function useOpenBackgroundAgentsPane(): ((agentId?: string) => void) | undefined {
  return useContext(BackgroundAgentsContext).openPane;
}

export function isActiveAgent(agent: BackgroundAgent): boolean {
  return (
    agent.status === "running" ||
    agent.status === "stalled" ||
    agent.status === "requires_action" ||
    agent.status === "unknown"
  );
}

export interface WorkflowPhase {
  phase: string;
  agents: BackgroundAgent[];
}

/** A workflow's agents by phase, phases in the order they first started. */
export function workflowPhases(agents: BackgroundAgent[]): WorkflowPhase[] {
  const phases = new Map<string, BackgroundAgent[]>();
  for (const agent of agents) {
    const phase = agent.workflow?.phase ?? "";
    phases.set(phase, [...(phases.get(phase) ?? []), agent]);
  }
  return [...phases].map(([phase, list]) => ({ phase, agents: list }));
}
