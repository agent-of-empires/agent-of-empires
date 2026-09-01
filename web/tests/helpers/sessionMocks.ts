import type { SessionInteraction } from "../../src/lib/api";

interface MockSession {
  id: string;
  view?: string;
}

export function mockSessionsEnvelope<T extends MockSession>(envelope: { sessions: T[]; workspace_ordering: string[] }) {
  const sessionInteractions: Record<string, SessionInteraction> = {};
  for (const session of envelope.sessions) {
    sessionInteractions[session.id] = {
      context_resume: { state: "available" },
      attach: {
        state: "available",
        transport: session.view === "structured" ? "acp_websocket_v1" : "terminal_websocket_v1",
      },
    };
  }
  return { ...envelope, session_interactions: sessionInteractions };
}
