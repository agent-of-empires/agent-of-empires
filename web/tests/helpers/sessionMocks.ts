interface MockSession {
  id: string;
}

export function mockSessionsEnvelope<T extends MockSession>(envelope: { sessions: T[]; workspace_ordering: string[] }) {
  return {
    ...envelope,
    sessions: envelope.sessions.map((session) => ({
      ...session,
      context_resume: { state: "available" as const },
    })),
  };
}
