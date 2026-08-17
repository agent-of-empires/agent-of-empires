/* eslint-disable react-refresh/only-export-components */
// React context exposing the active session's AgentProfile to the
// structured view's deeply-nested renderers. The wizard's `tool` field flows
// into `SessionResponse.tool`, which the structured view reads at view-mount
// time and feeds to the provider; classifiers in ToolCards consume it
// via `useAgentProfile()`.
//
// The conversation-reset slash aliases (claude `/clear`, codex/opencode
// `/new`) are server-owned: they arrive on `SessionResponse.clear_aliases`
// and are published through a sibling context so the composer palette and the
// queued-prompt clear-boundary hint read one source of truth instead of a
// per-agent mirror bundled with the classifier profile.
//
// Default value is `DEFAULT_AGENT_PROFILE` so a stray render outside
// the provider keeps the generic-tool dispatch working rather than
// throwing.

import { createContext, useContext, type ReactNode } from "react";

import { DEFAULT_AGENT_PROFILE, resolveAgentProfile, type AgentProfile } from "./agentProfiles";

const AgentProfileContext = createContext<AgentProfile>(DEFAULT_AGENT_PROFILE);

// Stable empty default so a consumer outside the provider (or a session
// whose agent has no clear alias) never gets a fresh array each render.
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

/** Server-owned conversation-reset slash aliases for the active session's
 *  agent (`SessionResponse.clear_aliases`). Empty when the agent has no clear
 *  alias or when rendered outside a provider. */
export function useClearAliases(): readonly string[] {
  return useContext(ClearAliasesContext);
}
