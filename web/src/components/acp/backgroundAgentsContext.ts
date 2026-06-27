// Shares the session's background-agent list (and a "open the Sub agents
// pane" callback) with the transcript, so an inline async Task card can
// reflect its live panel entry and jump to it. StructuredView publishes
// `ctx.state.backgroundAgents` plus the App-provided pane opener here; the
// panel itself reads the same data via the `useBackgroundAgents` store.

import { createContext, useContext } from "react";

import type { BackgroundAgent } from "../../lib/acpTypes";

export interface BackgroundAgentsContextValue {
  agents: BackgroundAgent[];
  /** Open (or focus) the Sub agents dock pane. Absent when no opener is
   *  wired (e.g. tests, or a surface without docks). */
  openPane?: () => void;
}

export const BackgroundAgentsContext = createContext<BackgroundAgentsContextValue>({ agents: [] });

/** The background agent launched by a given Task tool call, if any. */
export function useBackgroundAgentFor(toolCallId: string | undefined): BackgroundAgent | undefined {
  const { agents } = useContext(BackgroundAgentsContext);
  if (!toolCallId) return undefined;
  return agents.find((a) => a.toolCallId === toolCallId);
}

/** Callback to open the Sub agents pane, or undefined when not wired. */
export function useOpenBackgroundAgentsPane(): (() => void) | undefined {
  return useContext(BackgroundAgentsContext).openPane;
}
