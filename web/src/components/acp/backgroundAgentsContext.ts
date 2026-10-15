// Background agents and the pane opener, so an inline async Task card can show its live entry.

import { createContext, useContext } from "react";

import type { BackgroundAgent } from "../../lib/acpTypes";

export interface BackgroundAgentsContextValue {
  agents: BackgroundAgent[];
  openPane?: () => void;
}

export const BackgroundAgentsContext = createContext<BackgroundAgentsContextValue>({ agents: [] });

export function useBackgroundAgentFor(toolCallId: string | undefined): BackgroundAgent | undefined {
  const { agents } = useContext(BackgroundAgentsContext);
  if (!toolCallId) return undefined;
  return agents.find((a) => a.toolCallId === toolCallId);
}

export function useOpenBackgroundAgentsPane(): (() => void) | undefined {
  return useContext(BackgroundAgentsContext).openPane;
}
