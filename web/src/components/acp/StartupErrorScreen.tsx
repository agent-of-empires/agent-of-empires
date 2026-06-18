import { useEffect, useState } from "react";

import type { IncompatibleAgentDetail } from "../../lib/acpTypes";
import { fetchSettings, installAcpAgent } from "../../lib/api";
import { useRespawnSession } from "../../hooks/useRespawnSession";

interface Props {
  detail: IncompatibleAgentDetail;
  sessionId: string;
}

/** Dedicated full-region replacement for the structured view chat layout when
 *  the per-adapter compatibility check refuses the session. Distinct
 *  from `StartupErrorBanner`, which is a smaller text-based hint
 *  layered on top of the chat for free-form handshake failures. This
 *  screen surfaces the structured detail (installed vs required
 *  version, the exact remediation command) so the user can copy-paste
 *  it into a shell without parsing prose, and offers in-UI recovery:
 *  "Restart agent" respawns the worker (re-running the handshake after a
 *  manual reinstall), and, when the agent is npm-installable and the
 *  `acp.allow_agent_install` setting is on, "Update & restart" runs the
 *  install on the host then respawns. The install is global, so it also
 *  queues every other session blocked on the same adapter for an automatic
 *  respawn (reported as `recovered_sessions`), clearing every red X from
 *  one click. When the setting is off the button is shown disabled with a
 *  hint to enable it in the TUI (it is `local_only`, so the web cannot flip
 *  it). See #2109. */
export function StartupErrorScreen({ detail, sessionId }: Props) {
  const heading = headingFor(detail);
  const summary = summaryFor(detail);
  const installCommand = installCommandFor(detail);
  const autoInstallable = "auto_install" in detail && detail.auto_install;

  const { state: respawnState, error: respawnError, respawn } = useRespawnSession(sessionId);
  const [allowInstall, setAllowInstall] = useState(false);
  const [installState, setInstallState] = useState<"idle" | "installing" | "failed">("idle");
  const [installError, setInstallError] = useState<string | null>(null);
  const [installOutput, setInstallOutput] = useState<string | null>(null);
  const [recoveredCount, setRecoveredCount] = useState(0);

  useEffect(() => {
    let alive = true;
    fetchSettings().then((s) => {
      if (alive) {
        setAllowInstall(Boolean((s as { acp?: { allow_agent_install?: boolean } } | null)?.acp?.allow_agent_install));
      }
    });
    return () => {
      alive = false;
    };
  }, []);

  const busy = respawnState === "retrying" || installState === "installing";

  const handleUpdateAndRestart = async () => {
    setInstallState("installing");
    setInstallError(null);
    setInstallOutput(null);
    try {
      const res = await installAcpAgent(sessionId);
      const log = `${res.stdout}\n${res.stderr}`.trim();
      setInstallOutput(log || null);
      if (!res.success) {
        setInstallState("failed");
        setInstallError(`Install exited with code ${res.exit_code ?? "unknown"}.`);
        return;
      }
      setRecoveredCount(res.recovered_sessions);
      setInstallState("idle");
      // Re-run the handshake against the freshly installed version.
      await respawn();
    } catch (e) {
      setInstallState("failed");
      setInstallError(e instanceof Error ? e.message : String(e));
    }
  };

  return (
    <div
      role="alert"
      aria-live="assertive"
      data-testid="startup-error-screen"
      className="flex h-full flex-1 items-center justify-center bg-surface-900 px-4 py-12"
    >
      <div className="w-full max-w-xl rounded-lg border border-status-error/60 bg-surface-850 p-6 text-text-primary shadow-lg">
        <div className="text-[11px] font-semibold uppercase tracking-wide text-status-error">
          Adapter compatibility check failed
        </div>
        <h2 className="mt-2 text-lg font-semibold text-text-primary">{heading}</h2>
        <p className="mt-3 text-sm text-text-secondary">{summary}</p>

        {installCommand && (
          <div className="mt-4">
            <div className="text-[11px] font-medium uppercase tracking-wide text-text-dim">
              Run this, then restart the agent
            </div>
            <pre
              data-testid="startup-error-install-command"
              className="mt-1 overflow-x-auto rounded-md border border-surface-700 bg-surface-950 p-2 font-mono text-[12px] text-text-primary"
            >
              {installCommand}
            </pre>
          </div>
        )}

        <DetailRows detail={detail} />

        <div className="mt-5 flex flex-wrap items-center gap-2">
          <button
            type="button"
            data-testid="startup-error-restart"
            onClick={respawn}
            disabled={busy}
            className="rounded-md border border-surface-700 bg-surface-800 px-3 py-1.5 text-xs font-medium text-text-primary hover:bg-surface-700 disabled:cursor-not-allowed disabled:opacity-60"
          >
            {respawnState === "retrying" ? "Restarting…" : "Restart agent"}
          </button>
          {autoInstallable && allowInstall && (
            <button
              type="button"
              data-testid="startup-error-update-restart"
              onClick={handleUpdateAndRestart}
              disabled={busy}
              className="rounded-md border border-status-error/60 bg-status-error/20 px-3 py-1.5 text-xs font-medium text-text-primary hover:bg-status-error/30 disabled:cursor-not-allowed disabled:opacity-60"
            >
              {installState === "installing" ? "Updating…" : "Update & restart"}
            </button>
          )}
          {autoInstallable && !allowInstall && (
            <button
              type="button"
              data-testid="startup-error-update-restart-disabled"
              disabled
              title="Enable acp.allow_agent_install in the aoe TUI settings (Advanced) to install from the dashboard"
              className="cursor-not-allowed rounded-md border border-surface-700 bg-surface-800 px-3 py-1.5 text-xs font-medium text-text-dim opacity-60"
            >
              Update & restart
            </button>
          )}
        </div>

        {autoInstallable && !allowInstall && (
          <div className="mt-2 text-xs text-text-dim" data-testid="startup-error-enable-hint">
            One-click install is off. Enable{" "}
            <code className="rounded bg-surface-950 px-1 font-mono text-[12px]">acp.allow_agent_install</code> in the{" "}
            <code className="rounded bg-surface-950 px-1 font-mono text-[12px]">aoe</code> TUI settings (Advanced) to
            run the update from here. It is blocked from the web on purpose: it runs{" "}
            <code className="rounded bg-surface-950 px-1 font-mono text-[12px]">npm install</code> on the host.
          </div>
        )}

        {respawnState === "ok" && (
          <div className="mt-2 text-xs text-emerald-200/90">
            Restart requested. The agent re-runs the compatibility check on the next handshake.
            {recoveredCount > 0 &&
              ` Also restarting ${recoveredCount} other session${recoveredCount === 1 ? "" : "s"} blocked on the same adapter.`}
          </div>
        )}
        {respawnState === "failed" && respawnError && (
          <div className="mt-2 text-xs text-status-error">Restart failed: {respawnError}</div>
        )}
        {installState === "failed" && installError && (
          <div className="mt-2 text-xs text-status-error">{installError}</div>
        )}
        {installOutput && (
          <pre className="mt-2 max-h-40 overflow-auto rounded-md border border-surface-700 bg-surface-950 p-2 font-mono text-[11px] text-text-secondary">
            {installOutput}
          </pre>
        )}

        <div className="mt-4 text-xs text-text-dim">
          The session is paused until the adapter satisfies the required version. After installing, use{" "}
          <span className="font-medium text-text-secondary">Restart agent</span> above (no full restart of{" "}
          <code className="rounded bg-surface-950 px-1 font-mono text-[12px]">aoe serve</code> needed) and the check
          re-runs at the next ACP <code className="rounded bg-surface-950 px-1 font-mono text-[12px]">initialize</code>{" "}
          handshake.
        </div>
      </div>
    </div>
  );
}

function headingFor(detail: IncompatibleAgentDetail): string {
  switch (detail.kind) {
    case "incompatible_agent_version":
      return `${detail.package_name} ${detail.installed} is below the required ${detail.required}`;
    case "missing_agent_info":
      return "Adapter did not report its package version";
    case "mismatched_agent_name":
      return `Adapter package name does not match the expected agent`;
    case "unparseable_agent_version":
      return `Adapter reported an invalid version string`;
    case "unsupported_protocol_version":
      return `Adapter speaks an unsupported ACP protocol version`;
  }
}

function summaryFor(detail: IncompatibleAgentDetail): string {
  switch (detail.kind) {
    case "incompatible_agent_version":
      return `aoe requires ${detail.package_name} version ${detail.required} or newer. The installed adapter is ${detail.installed}. Updating the adapter unblocks the session; aoe relies on behavior (memory_recall tool calls, native cancelled stop reason, others) that older versions do not emit.`;
    case "missing_agent_info":
      return `aoe expected ${detail.expected_package} to report a package version in its ACP initialize response. The adapter returned an empty agent_info block, which usually means a stale install or a wrapper that strips metadata. Reinstall to the pinned version.`;
    case "mismatched_agent_name":
      return `aoe expected the adapter to identify itself as ${detail.expected} but it reported ${detail.received}. This usually means a wrapper script or a stale binary is on PATH. Reinstall the official adapter at the pinned version.`;
    case "unparseable_agent_version":
      return `aoe expected ${detail.package_name} to report a semver-compatible version string but received ${detail.raw_version}. Required minimum is ${detail.required}. Reinstall the official build to recover.`;
    case "unsupported_protocol_version":
      return `aoe negotiated ACP protocol ${detail.expected} but the adapter reported ${detail.received}. The session cannot proceed. This usually means an older or newer adapter generation than aoe currently supports.`;
  }
}

function installCommandFor(detail: IncompatibleAgentDetail): string | null {
  switch (detail.kind) {
    case "incompatible_agent_version":
    case "missing_agent_info":
    case "mismatched_agent_name":
    case "unparseable_agent_version":
      return detail.install_command;
    case "unsupported_protocol_version":
      return null;
  }
}

function DetailRows({ detail }: { detail: IncompatibleAgentDetail }) {
  const rows: Array<[string, string]> = [];
  switch (detail.kind) {
    case "incompatible_agent_version":
      rows.push(["Package", detail.package_name]);
      rows.push(["Installed", detail.installed]);
      rows.push(["Required", `>=${detail.required}`]);
      break;
    case "missing_agent_info":
      rows.push(["Expected package", detail.expected_package]);
      break;
    case "mismatched_agent_name":
      rows.push(["Expected", detail.expected]);
      rows.push(["Received", detail.received]);
      break;
    case "unparseable_agent_version":
      rows.push(["Package", detail.package_name]);
      rows.push(["Raw version", detail.raw_version]);
      rows.push(["Required", `>=${detail.required}`]);
      break;
    case "unsupported_protocol_version":
      rows.push(["Expected protocol", detail.expected]);
      rows.push(["Received protocol", detail.received]);
      break;
  }
  return (
    <dl className="mt-4 grid grid-cols-[max-content_1fr] gap-x-3 gap-y-1 font-mono text-[11px] text-text-secondary">
      {rows.map(([label, value]) => (
        <div key={label} className="contents">
          <dt className="font-medium text-text-dim">{label}</dt>
          <dd className="break-all">{value}</dd>
        </div>
      ))}
    </dl>
  );
}
