/* eslint-disable react-refresh/only-export-components */
// The active session's AgentProfile for deep structured view renderers, plus the server-owned clear aliases in a sibling context. Outside a provider it defaults to the generic profile.

import { createContext, useContext, type ReactNode } from "react";

import { DEFAULT_AGENT_PROFILE, resolveAgentProfile, type AgentProfile } from "./agentProfiles";

const AgentProfileContext = createContext<AgentProfile>(DEFAULT_AGENT_PROFILE);

// Stable empty default so consumers never see a fresh array.
const NO_CLEAR_ALIASES: readonly string[] = [];
const ClearAliasesContext = createContext<readonly string[]>(NO_CLEAR_ALIASES);

export function AgentProfileProvider({
  toolKey,
  clearAliases,
  children,
}: {
  toolKey: string | null | undefined;
  clearAliases?: readonly string[];
  children: ReactNode;
}) {
  const profile = resolveAgentProfile(toolKey);
  return (
    <AgentProfileContext.Provider value={profile}>
      <ClearAliasesContext.Provider value={clearAliases ?? NO_CLEAR_ALIASES}>{children}</ClearAliasesContext.Provider>
    </AgentProfileContext.Provider>
  );
}

export function useAgentProfile(): AgentProfile {
  return useContext(AgentProfileContext);
}

/** Empty without a clear alias or outside a provider. */
export function useClearAliases(): readonly string[] {
  return useContext(ClearAliasesContext);
}
