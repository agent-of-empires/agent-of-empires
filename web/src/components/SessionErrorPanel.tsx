import { useEffect, useRef, useState } from "react";

import { restartSession } from "../lib/api";
import type { SessionResponse } from "../lib/types";
import { RecoveryActionRow, SECONDARY_BUTTON, type RecoveryAction } from "./acp/RecoveryActionRow";
import { useCopy } from "./acp/useCopy";

interface Props {
  session: SessionResponse;
  /** Read-only dashboards show the error but can't restart. */
  readOnly: boolean;
}

/**
 * Shown over the terminal area of an Error Session instead of starting it, so opening the
 * Session doesn't erase the error it reports (#5055). The text is AoE's own, shown verbatim.
 */
export function SessionErrorPanel({ session, readOnly }: Props) {
  const lastError = session.last_error?.trim() ? session.last_error : null;
  const deleting = session.status === "Deleting";
  const canRestart = !readOnly && !deleting;
  const name = session.title.trim() || "This Session";
  const [restart, setRestart] = useState<{ state: "idle" | "restarting" } | { state: "failed"; message: string }>({
    state: "idle",
  });
  const [copied, copy] = useCopy();
  const mounted = useRef(true);
  useEffect(() => {
    // Set again in setup: Strict Mode runs the cleanup once before the real mount.
    mounted.current = true;
    return () => {
      mounted.current = false;
    };
  }, []);

  const onRestart = async () => {
    setRestart({ state: "restarting" });
    const result = await restartSession(session.id);
    if (!mounted.current) return;
    // A started Session leaves Error, which replaces this panel with the terminal.
    setRestart(
      result.ok ? { state: "idle" } : { state: "failed", message: result.message ?? "The Daemon didn't answer." },
    );
  };

  const restartAction: RecoveryAction = {
    label: restart.state === "restarting" ? "Restarting…" : "Restart",
    onClick: () => void onRestart(),
    testId: "session-error-restart",
    disabled: restart.state === "restarting",
  };
  const copyAction: RecoveryAction = {
    label: copied ? "Copied" : "Copy error",
    onClick: () => void copy(lastError ?? ""),
    testId: "session-error-copy",
  };

  return (
    <div
      data-testid="session-error-panel"
      data-recorded={lastError ? "true" : "false"}
      className="flex h-full flex-1 items-center justify-center bg-surface-900 px-4 py-12"
    >
      <div className="w-full max-w-xl rounded-lg border border-status-error/60 bg-surface-850 p-4 text-text-primary shadow-lg sm:p-6">
        <h2 className="text-lg font-semibold text-text-primary">
          <span className="font-mono">{name}</span> reported an error
        </h2>
        <p className="mt-1 text-sm text-text-subtle">
          {deleting
            ? "This Session is being deleted."
            : lastError
              ? "This is the most recent error AoE recorded for this Session."
              : canRestart
                ? "No details were recorded. Restart to try again."
                : "No details were recorded."}
        </p>
        {lastError && (
          <pre
            data-testid="session-error-text"
            className="mt-3 max-h-64 overflow-auto whitespace-pre-wrap break-all rounded-md border border-surface-700 bg-surface-950 p-2 font-mono text-xs text-text-primary"
          >
            {lastError}
          </pre>
        )}
        {canRestart ? (
          <RecoveryActionRow
            primary={restartAction}
            secondary={lastError ? [copyAction] : []}
            install={{ error: null, output: null }}
          />
        ) : (
          lastError && (
            <div className="mt-3">
              <button
                type="button"
                data-testid={copyAction.testId}
                onClick={copyAction.onClick}
                className={SECONDARY_BUTTON}
              >
                {copyAction.label}
              </button>
            </div>
          )
        )}
        {restart.state === "failed" && (
          <p data-testid="session-error-restart-failed" className="mt-2 text-xs text-status-error-text">
            Restart failed: {restart.message}
          </p>
        )}
      </div>
    </div>
  );
}
