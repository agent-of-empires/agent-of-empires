// Shares the session's background-agent list with the transcript so an
// inline async Task card can link to its live panel entry (status,
// elapsed, activity) without each card opening its own subscription.
// StructuredView publishes `ctx.state.backgroundAgents` here; the panel
// itself reads the same data via the `useBackgroundAgents` store.

import { createContext, useContext } from "react";

import type { BackgroundAgent } from "../../lib/acpTypes";

export const BackgroundAgentsContext = createContext<BackgroundAgent[]>([]);

/** The background agent launched by a given Task tool call, if any. */
export function useBackgroundAgentFor(toolCallId: string | undefined): BackgroundAgent | undefined {
  const agents = useContext(BackgroundAgentsContext);
  if (!toolCallId) return undefined;
  return agents.find((a) => a.toolCallId === toolCallId);
}
