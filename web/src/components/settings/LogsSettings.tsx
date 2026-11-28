import { useCallback, useEffect, useState } from "react";

import { fetchLogs, type LogsSnapshot } from "../../lib/api";
import { BUTTON_GHOST, BUTTON_PRIMARY, CARD } from "../../lib/surfaces";
import { CODE_BLOCK } from "../../lib/surfaces";
import { PanelMessage, SectionHeading } from "../panelParts";

const LINES = 500;

/// Redacted tail of the server log. Secrets are stripped server-side; Download saves exactly what is shown.
export function LogsSettings() {
  const [snapshot, setSnapshot] = useState<LogsSnapshot | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);

  const load = useCallback(async (signal?: AbortSignal) => {
    setBusy(true);
    setError(null);
    try {
      setSnapshot(await fetchLogs(LINES, signal));
    } catch (e) {
      if (signal?.aborted) return;
      setSnapshot(null);
      setError(e instanceof Error ? e.message : "Could not load logs");
    } finally {
      if (!signal?.aborted) setBusy(false);
    }
  }, []);

  useEffect(() => {
    const controller = new AbortController();
    // Deferred so the load's setState is not synchronous in the effect body.
    const first = setTimeout(() => void load(controller.signal), 0);
    return () => {
      clearTimeout(first);
      controller.abort();
    };
  }, [load]);

  function download() {
    if (!snapshot?.content) return;
    const url = URL.createObjectURL(new Blob([snapshot.content], { type: "text/plain" }));
    const link = document.createElement("a");
    link.href = url;
    link.download = `aoe-logs-${new Date().toISOString().replace(/[:.]/g, "-")}.log`;
    link.click();
    URL.revokeObjectURL(url);
  }

  const empty =
    snapshot?.status === "stdout"
      ? "Logging is set to stdout, so there is no file to show."
      : snapshot?.status === "unavailable"
        ? "This server has no active log file."
        : snapshot?.status === "missing"
          ? "No log file yet."
          : snapshot && !snapshot.content
            ? "The log file is empty."
            : null;

  return (
    <div>
      <SectionHeading
        title="Server log"
        description={
          <>
            The last {LINES} lines of the log file. Tokens, passphrases, API keys, and authorization headers are
            redacted on the server.
            {snapshot?.truncated && " Older lines are not shown."}
          </>
        }
      />

      <div className={`${CARD} py-4 space-y-3`}>
        <div className="flex flex-wrap items-center gap-2">
          <button
            type="button"
            className={BUTTON_PRIMARY}
            disabled={busy}
            onClick={() => void load()}
            data-testid="logs-refresh"
          >
            {busy ? "Loading…" : "Refresh"}
          </button>
          <button
            type="button"
            className={BUTTON_GHOST}
            disabled={!snapshot?.content}
            onClick={download}
            data-testid="logs-download"
          >
            Download
          </button>
        </div>

        {error && <PanelMessage tone="error">{error}</PanelMessage>}
        {empty && <PanelMessage>{empty}</PanelMessage>}

        {snapshot?.content && (
          <pre className={`${CODE_BLOCK} max-h-[32rem]`} data-testid="logs-content">
            {snapshot.content}
          </pre>
        )}
      </div>
    </div>
  );
}
