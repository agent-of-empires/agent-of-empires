/* eslint-disable react-refresh/only-export-components */
// React context exposing the active session's AgentProfile to the
// structured view's deeply-nested renderers. The wizard's `tool` field flows
// into `SessionResponse.tool`, which the structured view reads at view-mount
// time and feeds to the provider; classifiers in ToolCards consume it
// via `useAgentProfile()`.
//
// Default value is `DEFAULT_AGENT_PROFILE` so a stray render outside
// the provider keeps the generic-tool dispatch working rather than
// throwing.

import { createContext, useContext, type ReactNode } from "react";

import {
  DEFAULT_AGENT_PROFILE,
  resolveAgentProfile,
  type AgentProfile,
} from "./agentProfiles";

const AgentProfileContext = createContext<AgentProfile>(DEFAULT_AGENT_PROFILE);

export function AgentProfileProvider({
  toolKey,
  children,
}: {
  toolKey: string | null | undefined;
  children: ReactNode;
}) {
  const profile = resolveAgentProfile(toolKey);
  return (
    <AgentProfileContext.Provider value={profile}>
      {children}
    </AgentProfileContext.Provider>
  );
}

export function useAgentProfile(): AgentProfile {
  return useContext(AgentProfileContext);
}
