export function customDefaultAcpAgent(
  settings: Record<string, unknown> | null | undefined,
): string {
  const acp = settings?.acp as Record<string, unknown> | undefined;
  const session = settings?.session as Record<string, unknown> | undefined;
  const defaultAgent =
    typeof acp?.default_agent === "string" ? acp.default_agent.trim() : "";
  const agentAcpCmd = session?.agent_acp_cmd as
    | Record<string, unknown>
    | undefined;
  return defaultAgent && typeof agentAcpCmd?.[defaultAgent] === "string"
    ? defaultAgent
    : "";
}
