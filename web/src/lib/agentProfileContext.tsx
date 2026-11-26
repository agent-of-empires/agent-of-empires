/* eslint-disable react-refresh/only-export-components */
// The active session's AgentProfile for deep structured view renderers. Outside a provider it defaults to the generic profile.

import { createContext, useContext, type ReactNode } from "react";

import { DEFAULT_AGENT_PROFILE, type AgentProfile } from "./agentProfile";

const AgentProfileContext = createContext<AgentProfile>(DEFAULT_AGENT_PROFILE);

export function AgentProfileProvider({
  profile,
  children,
}: {
  profile: AgentProfile | null | undefined;
  children: ReactNode;
}) {
  return (
    <AgentProfileContext.Provider value={profile ?? DEFAULT_AGENT_PROFILE}>{children}</AgentProfileContext.Provider>
  );
}

export function useAgentProfile(): AgentProfile {
  return useContext(AgentProfileContext);
}
