// Background agents and the pane opener, so an inline async Task or Workflow card can show its live entries.

import { createContext, useContext, useMemo } from "react";

import type { BackgroundAgent, WorkflowRun } from "../../lib/acpTypes";

export interface BackgroundAgentsContextValue {
  agents: BackgroundAgent[];
  workflowRuns?: Record<string, WorkflowRun>;
  backgroundTasks?: string[];
  openPane?: () => void;
}

export const BackgroundAgentsContext = createContext<BackgroundAgentsContextValue>({ agents: [] });

export function useBackgroundAgentFor(toolCallId: string | undefined): BackgroundAgent | undefined {
  const { agents } = useContext(BackgroundAgentsContext);
  if (!toolCallId) return undefined;
  return agents.find((a) => a.toolCallId === toolCallId);
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
  return run ? backgroundTasks.includes(run.taskId) : agents.some(isActiveAgent);
}

export function useOpenBackgroundAgentsPane(): (() => void) | undefined {
  return useContext(BackgroundAgentsContext).openPane;
}

export function isActiveAgent(agent: BackgroundAgent): boolean {
  return agent.status === "running" || agent.status === "stalled";
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
