import { useCallback, useEffect, useState } from "react";

import { fetchLogs, type LogsSnapshot } from "../../lib/api";

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
    <div className="space-y-4">
      <div>
        <div className="text-[13px] text-text-secondary">Server log</div>
        <p className="mt-1 text-[11px] text-text-muted">
          The last {LINES} lines of the log file. Tokens, passphrases, API keys, and authorization headers are redacted
          on the server.
          {snapshot?.truncated && " Older lines are not shown."}
        </p>
      </div>

      <div className="flex flex-wrap items-center gap-2">
        <button
          type="button"
          className="h-8 cursor-pointer rounded-md bg-brand-600 px-3 text-xs font-medium text-white transition-colors duration-150 hover:bg-brand-500 disabled:opacity-50"
          disabled={busy}
          onClick={() => void load()}
          data-testid="logs-refresh"
        >
          {busy ? "Loading…" : "Refresh"}
        </button>
        <button
          type="button"
          className="h-8 cursor-pointer rounded-md px-3 text-xs text-text-secondary transition-colors duration-150 hover:bg-surface-800 hover:text-text-primary disabled:opacity-50"
          disabled={!snapshot?.content}
          onClick={download}
          data-testid="logs-download"
        >
          Download
        </button>
      </div>

      {error && <p className="text-[11px] text-status-error">{error}</p>}
      {empty && <p className="text-[11px] text-text-dim">{empty}</p>}

      {snapshot?.content && (
        <pre
          className="max-h-[32rem] overflow-auto rounded border border-surface-700 bg-surface-900 p-3 text-[11px] leading-relaxed text-text-secondary"
          data-testid="logs-content"
        >
          {snapshot.content}
        </pre>
      )}
    </div>
  );
}
