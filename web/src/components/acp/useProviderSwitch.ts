import { useCallback, useState } from "react";

import { switchAcpProvider } from "../../lib/api";
import { reportError } from "../../lib/toastBus";

/** What this tab picked, standing in until the session row reports it, and
 *  the row value it replaced. */
type ProviderEcho = { sessionId: string; value: string; from: string | null } | null;

/** The routing flags only mean something to Claude, so the picker is absent
 *  for every other agent and the server refuses the call anyway.
 *
 *  The switch has no event of its own, so `acp_provider` on the session row
 *  only catches up on the next poll. The echo is retired the first render the
 *  row moves off the value it replaced, whether to confirm the pick or because
 *  another tab or the CLI switched, so the row going back to that value later
 *  cannot revive it. */
export function useProviderSwitch(sessionId: string, agent: string | null, serverProvider: string | null) {
  const [pending, setPending] = useState<ProviderEcho>(null);
  const [accepted, setAccepted] = useState<ProviderEcho>(null);
  const supported = agent === "claude" || agent === "claude-code";

  const live = (echo: ProviderEcho) =>
    echo && echo.sessionId === sessionId && echo.from === serverProvider ? echo.value : null;
  if (accepted && live(accepted) === null) setAccepted(null);

  const set = useCallback(
    async (next: string) => {
      setPending({ sessionId, value: next, from: serverProvider });
      try {
        const result = await switchAcpProvider(sessionId, next);
        setAccepted({ sessionId, value: result.provider, from: serverProvider });
      } catch (e) {
        reportError(`Provider switch failed: ${e instanceof Error ? e.message : String(e)}`);
      } finally {
        setPending(null);
      }
    },
    [sessionId, serverProvider],
  );

  if (!supported) return { current: null, pending: null, set: undefined };
  return { current: live(accepted) ?? serverProvider, pending: live(pending), set };
}
