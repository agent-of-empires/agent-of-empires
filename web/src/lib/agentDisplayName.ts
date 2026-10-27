import { useSyncExternalStore } from "react";
import { fetchAgents } from "./api";
import type { AgentInfo } from "./types";

const NONE: ReadonlyMap<string, string> = new Map();
let names = NONE;
let requested = false;
const listeners = new Set<() => void>();

/** Replaces the names with the server's `display_name`s; an empty list is a failed fetch and is ignored. */
export function setAgentNames(agents: AgentInfo[]) {
  if (agents.length === 0) return;
  // A Map, so custom registry keys such as `constructor` never hit Object.prototype.
  names = new Map(agents.map((a) => [a.name, a.display_name]));
  listeners.forEach((l) => l());
}

function subscribe(listener: () => void) {
  listeners.add(listener);
  if (!requested) {
    requested = true;
    fetchAgents().then((agents) => {
      setAgentNames(agents);
      requested = agents.length > 0;
    });
  }
  return () => void listeners.delete(listener);
}

/** How copy names an Agent: the server's name, else the registry id. */
export function resolveAgentDisplayName(agent: string | null | undefined, known: ReadonlyMap<string, string>): string {
  if (!agent) return "the Agent";
  return known.get(agent) ?? agent;
}

/** A resolver that re-renders its component once the server's names arrive. */
export function useAgentDisplayNames(): (agent: string | null | undefined) => string {
  const known = useSyncExternalStore(subscribe, () => names);
  return (agent) => resolveAgentDisplayName(agent, known);
}
