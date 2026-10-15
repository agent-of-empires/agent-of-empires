import { useState } from "react";

import { useRespawnSession } from "../../hooks/useRespawnSession";

function Code({ children }: { children: React.ReactNode }) {
  return <code className="rounded bg-rose-900/60 px-1">{children}</code>;
}

function Remediation({ message }: { message: string }) {
  if (/authentic|login|api[_ -]?key/i.test(message)) {
    return (
      <>
        The adapter is installed but has no Claude credentials. Either set <Code>ANTHROPIC_API_KEY</Code> in the env
        that runs <Code>aoe serve</Code>, or run <Code>claude /login</Code> in a terminal to write credentials to{" "}
        <Code>~/.claude</Code>, then restart aoe.
      </>
    );
  }
  if (/capacity full|max_concurrent_workers/i.test(message)) {
    return (
      <>
        All structured view worker slots are in use. Either raise <Code>[acp] max_concurrent_workers</Code> in{" "}
        <Code>config.toml</Code> and restart <Code>aoe serve</Code>, or free a slot by deleting an existing structured
        view session or switching one to the tmux view. Reinstalling the adapter won't help; the adapter is fine, the
        cap is the limit.
      </>
    );
  }
  // Matches the Display of `AcpError::ProjectPathMissing`.
  const missing = /project path no longer exists:\s*(\S.*)$/im.exec(message);
  if (missing) {
    const missingPath = missing[1]?.trim();
    return (
      <>
        The session's working directory no longer exists on disk:
        {missingPath && (
          <pre className="mt-1 whitespace-pre-wrap break-all rounded bg-rose-900/40 p-2 text-xs">{missingPath}</pre>
        )}
        Reinstalling the adapter won't help; the adapter is fine, the cwd is gone. Two paths forward:
        <ol className="mt-1 list-decimal space-y-0.5 pl-5">
          <li>
            Restore the directory at the path above (e.g. <Code>git worktree move</Code> it back, or recreate it), then
            click <strong>Retry</strong>.
          </li>
          <li>
            Stop <Code>aoe serve</Code>, edit <Code>project_path</Code> for this session in{" "}
            <Code>~/.agent-of-empires/profiles/&lt;profile&gt;/sessions.json</Code> to point at the new location, then
            start <Code>aoe serve</Code> again.
          </li>
        </ol>
      </>
    );
  }
  if (/native binary at .* exists but failed to launch/i.test(message)) {
    return (
      <>
        The adapter is installed but its bundled Claude Code native sub-binary couldn't launch. The binary exists on
        disk, the kernel rejected the <Code>execve</Code>. Reinstalling the adapter won't help; the binary is already
        there. Likely causes:
        <ul className="mt-1 list-disc space-y-0.5 pl-5">
          <li>
            Architecture mismatch (e.g. an <Code>arm64</Code> binary inside an <Code>amd64</Code> sandbox container, or
            vice versa).
          </li>
          <li>Container image missing the dynamic loader or a glibc version old enough to refuse the binary.</li>
          <li>
            Host <Code>node_modules</Code> bind-mounted into a container of a different arch.
          </li>
        </ul>
        Open the agent log below for the verbatim adapter error, or see{" "}
        <a
          href="https://agent-of-empires.com/docs/structured-view/troubleshooting/#native-binary-launch-failure"
          target="_blank"
          rel="noreferrer"
          className="underline hover:text-rose-100"
        >
          the troubleshooting guide
        </a>
        .
      </>
    );
  }
  return (
    <>
      Run <Code>aoe acp doctor --fix</Code> from a terminal, or install the adapter manually:
      <pre className="mt-1 whitespace-pre-wrap rounded bg-rose-900/40 p-2 text-xs">
        npm install -g @agentclientprotocol/claude-agent-acp@latest
      </pre>
    </>
  );
}

export function StartupErrorBanner({ sessionId, message }: { sessionId: string; message: string }) {
  const { state: retryState, error: retryError, respawn: handleRetry } = useRespawnSession(sessionId);
  return (
    <div className="border-b border-rose-900/60 bg-rose-950/40 px-4 py-3 text-rose-200">
      <div className="flex items-start justify-between gap-3">
        <div className="flex-1 min-w-0">
          <div className="text-sm font-medium">Structured view agent failed to start</div>
          <pre className="mt-1 whitespace-pre-wrap text-xs text-rose-100/90">{message}</pre>
        </div>
        <button
          type="button"
          onClick={handleRetry}
          disabled={retryState === "retrying"}
          className="shrink-0 rounded-md border border-rose-800/60 bg-rose-900/40 px-3 py-1 text-xs font-medium text-rose-100 hover:bg-rose-900/60 disabled:cursor-not-allowed disabled:opacity-60"
        >
          {retryState === "retrying" ? "Retrying…" : "Retry"}
        </button>
      </div>
      {retryState === "ok" && (
        <div className="mt-2 text-xs text-emerald-200/90">
          Spawn requested. New events should start streaming in shortly.
        </div>
      )}
      {retryState === "failed" && retryError && (
        <div className="mt-2 text-xs text-rose-100/90">Retry failed: {retryError}</div>
      )}
      <div className="mt-2 text-xs text-rose-200/80">
        <Remediation message={message} />
      </div>
      <AgentLogDisclosure sessionId={sessionId} />
    </div>
  );
}

type LogState =
  | { kind: "idle" | "loading" }
  | { kind: "failed"; error: string }
  | { kind: "ok"; exists: boolean; tail: string; truncated: boolean };

/** Lazily fetched tail of the per-session runner log (`aoe acp logs`), for
 *  users without host terminal access. */
function AgentLogDisclosure({ sessionId }: { sessionId: string }) {
  const [open, setOpen] = useState(false);
  const [log, setLog] = useState<LogState>({ kind: "idle" });

  const fetchLog = async () => {
    setLog({ kind: "loading" });
    try {
      const res = await fetch(`/api/sessions/${encodeURIComponent(sessionId)}/acp/worker-log?tail=200`);
      if (!res.ok) {
        const detail = (await res.text().catch(() => "")).slice(0, 200);
        setLog({ kind: "failed", error: `Server returned ${res.status}. ${detail}`.trim() });
        return;
      }
      const body = (await res.json()) as { exists?: boolean; tail?: string; truncated?: boolean };
      setLog({
        kind: "ok",
        exists: Boolean(body.exists),
        tail: typeof body.tail === "string" ? body.tail : "",
        truncated: Boolean(body.truncated),
      });
    } catch (e) {
      setLog({ kind: "failed", error: e instanceof Error ? e.message : String(e) });
    }
  };

  const handleToggle = () => {
    setOpen(!open);
    if (!open && log.kind === "idle") void fetchLog();
  };

  return (
    <div className="mt-3 border-t border-rose-900/60 pt-2">
      <div className="flex items-center justify-between gap-2">
        <button
          type="button"
          onClick={handleToggle}
          data-testid="acp-agent-log-toggle"
          aria-expanded={open}
          className="text-xs font-medium text-rose-100 underline-offset-2 hover:underline"
        >
          {open ? "Hide agent log" : "Open agent log"}
        </button>
        {open && (
          <button
            type="button"
            onClick={() => void fetchLog()}
            disabled={log.kind === "loading"}
            data-testid="acp-agent-log-refresh"
            className="rounded-md border border-rose-800/60 bg-rose-900/40 px-2 py-0.5 text-[10px] font-medium text-rose-100 hover:bg-rose-900/60 disabled:cursor-not-allowed disabled:opacity-60"
          >
            {log.kind === "loading" ? "Loading…" : "Refresh"}
          </button>
        )}
      </div>
      {open && (
        <div className="mt-2" data-testid="acp-agent-log-body">
          {log.kind === "loading" && <div className="text-xs text-rose-200/80">Loading log…</div>}
          {log.kind === "failed" && log.error && (
            <div className="text-xs text-rose-100/90">Could not load log: {log.error}</div>
          )}
          {log.kind === "ok" && !log.exists && (
            <div className="text-xs text-rose-200/80">
              No log output yet. The worker may not have written anything before exiting.
            </div>
          )}
          {log.kind === "ok" && log.exists && log.tail.length === 0 && (
            <div className="text-xs text-rose-200/80">Log file exists but is empty.</div>
          )}
          {log.kind === "ok" && log.exists && log.tail.length > 0 && (
            <>
              {log.truncated && (
                <div className="mb-1 text-[10px] text-rose-200/70">Log is large; showing the tail.</div>
              )}
              <pre
                data-testid="acp-agent-log-pre"
                className="max-h-64 overflow-auto whitespace-pre-wrap break-all rounded bg-rose-950/70 p-2 font-mono text-[11px] text-rose-100/90"
              >
                {log.tail}
              </pre>
            </>
          )}
        </div>
      )}
    </div>
  );
}
