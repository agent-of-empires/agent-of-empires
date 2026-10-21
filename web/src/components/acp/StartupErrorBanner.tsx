import { useEffect, useState } from "react";

import { useRespawnSession } from "../../hooks/useRespawnSession";
import { fetchAgentInstallInfo, type AgentInstallInfo } from "../../lib/api";
import { writeClipboard } from "../../lib/clipboard";
import { splitInstallHint, startupErrorCause, useAdapterInstall } from "./adapterRecovery";
import { RecoveryActionRow, type RecoveryAction } from "./RecoveryActionRow";

function Code({ children }: { children: React.ReactNode }) {
  return <code className="rounded bg-surface-950 px-1 font-mono text-[12px]">{children}</code>;
}

/** Copy for the failures a reinstall won't fix, keyed off the message text. */
function specificRemediation(message: string): React.ReactNode {
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
  // The Daemon refuses a stale bundled copy or one its Node can't run; npm can't fix either.
  const stale = /bundled ACP adapter `([^`]+)` predates this aoe build/.exec(message);
  if (stale) {
    return (
      <>
        aoe's bundled copy of the ACP adapter is older than this aoe build, so the Daemon won't run it. Run{" "}
        <Code>aoe acp doctor --fix --adapter {stale[1]}</Code> where the Daemon runs, then click <strong>Retry</strong>.
      </>
    );
  }
  const oldNode = /needs Node (\S+) or newer to run its sources; found (.+?)\. Upgrade/.exec(message);
  if (oldNode) {
    return (
      <>
        The ACP adapter needs Node {oldNode[1]} or newer, and the Daemon found <Code>{oldNode[2]}</Code>. Upgrade Node,
        or point <Code>AOE_ACP_NODE</Code> at a newer one, then click <strong>Retry</strong>. Reinstalling the adapter
        won't help.
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
          <pre className="mt-1 whitespace-pre-wrap break-all rounded-md bg-surface-950 p-2 font-mono text-xs">
            {missingPath}
          </pre>
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
          className="underline hover:text-text-primary"
        >
          the troubleshooting guide
        </a>
        .
      </>
    );
  }
  return null;
}

/** The Agent named in a spawn failure ("Failed to start structured view agent \"x\""). */
const AGENT_IN_MESSAGE = /structured view agent "([^"]+)"/;
/** The Daemon's missing-adapter error, which names its cause. */
const ADAPTER_NOT_FOUND = /binary `[^`]+` not found/;

export function StartupErrorBanner({
  sessionId,
  message,
  onSwitchToTerminal,
}: {
  sessionId: string;
  message: string;
  /** Opens the switch-view confirmation; omitted where the Session can't switch. */
  onSwitchToTerminal?: () => void;
}) {
  const [info, setInfo] = useState<AgentInstallInfo | null>(null);
  const [infoRequest, setInfoRequest] = useState(0);
  const [detailsOpen, setDetailsOpen] = useState(false);
  const { state: retryState, detail: retryDetail, respawn } = useRespawnSession(sessionId);
  const install = useAdapterInstall(sessionId, respawn);

  useEffect(() => {
    let alive = true;
    void fetchAgentInstallInfo(sessionId).then((next) => {
      if (alive && next) setInfo(next);
    });
    return () => {
      alive = false;
    };
  }, [sessionId, infoRequest]);

  const name = info?.display_name ?? AGENT_IN_MESSAGE.exec(message)?.[1] ?? "The agent";
  const specific = specificRemediation(message);
  const canInstall = !specific && info?.install_allowed === true && info.npm_installable;
  const busy = retryState === "retrying" || install.state === "installing";

  const retry = async () => {
    // A setting changed in the TUI shows up on the next attempt.
    setInfoRequest((n) => n + 1);
    if (!(await respawn())) setDetailsOpen(true);
  };
  const retryAction: RecoveryAction = {
    label: retryState === "retrying" ? "Retrying…" : "Retry",
    onClick: () => void retry(),
    testId: "startup-error-banner-retry",
    disabled: busy,
  };
  const installAction: RecoveryAction = {
    label:
      install.state === "installing"
        ? "Installing…"
        : info?.sandboxed
          ? "Install in sandbox and retry"
          : "Install and retry",
    onClick: () => void install.install(),
    testId: "startup-error-banner-install",
    disabled: busy,
  };
  const switchAction: RecoveryAction[] = onSwitchToTerminal
    ? [{ label: "Use Terminal view", onClick: onSwitchToTerminal, testId: "startup-error-banner-terminal" }]
    : [];

  const retryFailure =
    retryState === "failed" && retryDetail !== null
      ? startupErrorCause(retryDetail) === startupErrorCause(message)
        ? "Retry failed: same error"
        : `Retry failed: ${startupErrorCause(retryDetail)}`
      : null;

  return (
    <section
      aria-label="Structured view startup error"
      data-testid="startup-error-banner"
      className="border-b border-status-error/40 bg-status-error/10 px-4 py-3 text-text-primary"
    >
      <div role="alert">
        <h2 className="text-sm font-semibold">{name} couldn't start in Structured view</h2>
        <div className="mt-1 text-xs text-text-secondary">
          {specific ??
            (ADAPTER_NOT_FOUND.test(message) ? (
              <>{name}'s ACP adapter isn't installed where the Daemon runs.</>
            ) : (
              <>
                The ACP adapter, the program that connects AoE to {name}, didn't start. It's usually missing or out of
                date.
              </>
            ))}
        </div>
      </div>
      {!specific && !canInstall && <InstallSteps info={info} />}
      <RecoveryActionRow
        primary={canInstall ? installAction : retryAction}
        secondary={[...(canInstall ? [retryAction] : []), ...switchAction]}
        install={install}
      />
      {retryState === "ok" && (
        <div className="mt-2 text-xs text-status-running">Retrying. Events will appear when {name} starts.</div>
      )}
      <div className="mt-3 border-t border-status-error/30 pt-2">
        <button
          type="button"
          data-testid="startup-error-banner-details"
          aria-expanded={detailsOpen}
          onClick={() => setDetailsOpen(!detailsOpen)}
          className="text-xs font-medium text-text-primary underline-offset-2 hover:underline"
        >
          Details
        </button>
        {detailsOpen && (
          <div className="mt-2" data-testid="startup-error-banner-details-body">
            <pre className="whitespace-pre-wrap break-all font-mono text-xs text-text-secondary">{message}</pre>
            {retryFailure && <div className="mt-1 font-mono text-xs text-text-secondary">{retryFailure}</div>}
            <AgentLogDisclosure sessionId={sessionId} />
          </div>
        )}
      </div>
    </section>
  );
}

/** What to run where, for an adapter the dashboard can't install itself. */
function InstallSteps({ info }: { info: AgentInstallInfo | null }) {
  if (!info?.install_command) {
    return (
      <p className="mt-2 text-xs text-text-secondary">
        Run <Code>aoe acp doctor --fix</Code> where the Daemon runs, then Retry.
      </p>
    );
  }
  const hint = splitInstallHint(info.install_command);
  if (info.sandboxed) {
    return (
      <div className="mt-2 text-xs text-text-secondary" data-testid="startup-error-banner-sandbox-note">
        <p>The ACP adapter lives inside the sandbox container, so installing it on the host won't reach it.</p>
        <p className="mt-2">Run this inside the container, then Retry:</p>
        <CommandBlock {...hint} />
        <p className="mt-2 text-text-dim">
          This updates the running container only. To keep new Sessions from hitting this, refresh the sandbox image:
          the aoe TUI shows a "sandbox image update available" banner that pulls the right image with your configured
          container runtime.
        </p>
      </div>
    );
  }
  return (
    <div className="mt-2 text-xs text-text-secondary">
      <CommandBlock {...hint} />
      <p className="mt-2">Run this where the Daemon runs, then Retry.</p>
    </div>
  );
}

function CommandBlock({ command, note }: { command: string; note: string | null }) {
  const [copied, setCopied] = useState(false);
  return (
    <>
      <div className="mt-1 flex items-start gap-2 rounded-md border border-surface-700 bg-surface-950 p-2">
        <code
          data-testid="startup-error-banner-command"
          className="min-w-0 flex-1 whitespace-pre-wrap break-all font-mono text-[12px] text-text-primary"
        >
          {command}
        </code>
        <button
          type="button"
          onClick={() => void writeClipboard(command).then(setCopied)}
          className="shrink-0 rounded-md px-2 text-[11px] text-text-secondary hover:text-text-primary"
        >
          {copied ? "Copied" : "Copy"}
        </button>
      </div>
      {note && <p className="mt-1 text-text-dim">{note}</p>}
    </>
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
    <div className="mt-2">
      <div className="flex items-center justify-between gap-2">
        <button
          type="button"
          onClick={handleToggle}
          data-testid="acp-agent-log-toggle"
          aria-expanded={open}
          className="text-xs font-medium text-text-primary underline-offset-2 hover:underline"
        >
          {open ? "Hide agent log" : "Open agent log"}
        </button>
        {open && (
          <button
            type="button"
            onClick={() => void fetchLog()}
            disabled={log.kind === "loading"}
            data-testid="acp-agent-log-refresh"
            className="rounded-md border border-surface-700 bg-surface-800 px-2 py-0.5 text-[10px] font-medium text-text-primary hover:bg-surface-700 disabled:cursor-not-allowed disabled:opacity-60"
          >
            {log.kind === "loading" ? "Loading…" : "Refresh"}
          </button>
        )}
      </div>
      {open && (
        <div className="mt-2" data-testid="acp-agent-log-body">
          {log.kind === "loading" && <div className="text-xs text-text-secondary">Loading log…</div>}
          {log.kind === "failed" && log.error && (
            <div className="text-xs text-text-secondary">Could not load log: {log.error}</div>
          )}
          {log.kind === "ok" && !log.exists && (
            <div className="text-xs text-text-secondary">
              No log output yet. The worker may not have written anything before exiting.
            </div>
          )}
          {log.kind === "ok" && log.exists && log.tail.length === 0 && (
            <div className="text-xs text-text-secondary">Log file exists but is empty.</div>
          )}
          {log.kind === "ok" && log.exists && log.tail.length > 0 && (
            <>
              {log.truncated && <div className="mb-1 text-[10px] text-text-dim">Log is large; showing the tail.</div>}
              <pre
                data-testid="acp-agent-log-pre"
                className="max-h-64 overflow-auto whitespace-pre-wrap break-all rounded bg-surface-950 p-2 font-mono text-[11px] text-text-secondary"
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
