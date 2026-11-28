/* eslint-disable react-refresh/only-export-components */
// The active session's AgentProfile for deep structured view renderers. Outside a provider it defaults to the generic profile.

import { createContext, useContext, useState, type ReactNode } from "react";

import { DEFAULT_AGENT_PROFILE, usableAgentProfile, type AgentProfile } from "./agentProfile";

const AgentProfileContext = createContext<AgentProfile>(DEFAULT_AGENT_PROFILE);

/** An unknown `v` falls back to the default; equal content keeps the previous object, as the session poll
 *  deserializes a fresh one each time. */
export function useStableAgentProfile(profile: AgentProfile | null | undefined): AgentProfile {
  const usable = usableAgentProfile(profile);
  const json = JSON.stringify(usable);
  const [held, setHeld] = useState({ json, profile: usable });
  if (held.json === json) return held.profile;
  setHeld({ json, profile: usable });
  return usable;
}

export function AgentProfileProvider({
  profile,
  children,
}: {
  profile: AgentProfile | null | undefined;
  children: ReactNode;
}) {
  const stable = useStableAgentProfile(profile);
  return <AgentProfileContext.Provider value={stable}>{children}</AgentProfileContext.Provider>;
}

export function useAgentProfile(): AgentProfile {
  return useContext(AgentProfileContext);
}
