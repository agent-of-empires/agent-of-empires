import { useEffect, useState } from "react";

import type { IncompatibleAgentDetail } from "../../lib/acpTypes";
import { fetchSettings } from "../../lib/api";
import { useRespawnSession } from "../../hooks/useRespawnSession";
import { useAdapterInstall } from "./adapterRecovery";
import { RecoveryActionRow, type RecoveryAction } from "./RecoveryActionRow";

interface Props {
  detail: IncompatibleAgentDetail;
  sessionId: string;
  /** The adapter lives in a container, so the host install command does not apply. */
  isSandboxed?: boolean;
}

/** Replaces the chat when the adapter compatibility check refuses the session. Shows
 *  the exact remediation, a respawn, and (with `acp.allow_agent_install`, which only the
 *  TUI can enable) an install that also respawns other sessions blocked on the adapter. */
export function StartupErrorScreen({ detail, sessionId, isSandboxed = false }: Props) {
  const heading = headingFor(detail);
  const summary = summaryFor(detail);
  const installCommand = installCommandFor(detail);
  const autoInstallable = "auto_install" in detail && detail.auto_install;
  const showHostCommand = installCommand && !isSandboxed;

  const { state: respawnState, error: respawnError, respawn } = useRespawnSession(sessionId);
  const [allowInstall, setAllowInstall] = useState(false);
  const install = useAdapterInstall(sessionId, respawn);

  useEffect(() => {
    let alive = true;
    fetchSettings().then((s) => {
      if (alive) {
        setAllowInstall(s?.acp?.allow_agent_install === true);
      }
    });
    return () => {
      alive = false;
    };
  }, []);

  const busy = respawnState === "retrying" || install.state === "installing";
  const restart: RecoveryAction = {
    label: respawnState === "retrying" ? "Restarting…" : "Restart agent",
    onClick: () => void respawn(),
    testId: "startup-error-restart",
    disabled: busy,
  };
  const update: RecoveryAction = {
    label:
      install.state === "installing" ? "Updating…" : isSandboxed ? "Update in sandbox & restart" : "Update & restart",
    onClick: () => void install.install(),
    testId: "startup-error-update-restart",
    disabled: busy,
  };
  const updateDisabled: RecoveryAction = {
    label: "Update & restart",
    onClick: () => {},
    testId: "startup-error-update-restart-disabled",
    disabled: true,
    title:
      'Turn on "Allow agent install from web" in the aoe TUI settings (Global, Acp tab, Advanced) to install from the dashboard',
  };
  const canUpdate = autoInstallable && allowInstall;

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
        <p className="mt-3 text-sm text-text-subtle">{summary}</p>

        {showHostCommand && (
          <div className="mt-4">
            <div className="text-[11px] font-medium uppercase tracking-wide text-text-subtle">
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

        {isSandboxed && installCommand && (
          <div className="mt-4" data-testid="startup-error-sandbox-note">
            <div className="text-[11px] font-medium uppercase tracking-wide text-text-subtle">
              This session runs in a sandbox
            </div>
            {autoInstallable ? (
              <p className="mt-1 text-sm text-text-subtle">
                The adapter lives inside the container, so a host{" "}
                <code className="rounded bg-surface-950 px-1 font-mono text-[12px]">npm install</code> would not reach
                it. Use <span className="font-medium text-text-subtle">Update &amp; restart</span> below to install the
                adapter <span className="font-medium text-text-subtle">inside the container</span> and reload the
                session.
              </p>
            ) : (
              <>
                <p className="mt-1 text-sm text-text-subtle">
                  The adapter lives inside the container, so installing it on the host would not reach it.
                </p>
                <div className="mt-3 text-[11px] font-medium uppercase tracking-wide text-text-subtle">
                  Run this inside the container, then press Restart agent
                </div>
                <pre
                  data-testid="startup-error-sandbox-install-command"
                  className="mt-1 overflow-x-auto rounded-md border border-surface-700 bg-surface-950 p-2 font-mono text-[12px] text-text-primary"
                >
                  {installCommand}
                </pre>
              </>
            )}
            <p className="mt-2 text-xs text-text-subtle">
              This updates the running container only. To keep new sessions from hitting this, update the sandbox image:
              when a newer one is published, the dashboard shows a "Sandbox image update available" banner at the top
              that pulls it with your Container runtime.
            </p>
          </div>
        )}

        <DetailRows detail={detail} />

        <div className="mt-2">
          <RecoveryActionRow
            primary={canUpdate ? update : restart}
            secondary={canUpdate ? [restart] : autoInstallable ? [updateDisabled] : []}
            install={install}
          />
        </div>

        {autoInstallable && !allowInstall && (
          <div className="mt-2 text-xs text-text-subtle" data-testid="startup-error-enable-hint">
            One-click install is off. Enable{" "}
            <code className="rounded bg-surface-950 px-1 font-mono text-[12px]">acp.allow_agent_install</code> in the{" "}
            <code className="rounded bg-surface-950 px-1 font-mono text-[12px]">aoe</code> TUI settings (Advanced) to
            run the update from here. It is blocked from the web on purpose: it runs{" "}
            <code className="rounded bg-surface-950 px-1 font-mono text-[12px]">npm install</code>{" "}
            {isSandboxed ? "inside the sandbox container" : "on the host"}.
          </div>
        )}

        {respawnState === "ok" && (
          <div className="mt-2 text-xs text-emerald-200/90">
            Restart requested. The agent re-runs the compatibility check on the next handshake.
            {install.recovered > 0 &&
              ` Also restarting ${install.recovered} other session${install.recovered === 1 ? "" : "s"} blocked on the same adapter.`}
          </div>
        )}
        {respawnState === "failed" && respawnError && (
          <div className="mt-2 text-xs text-status-error">Restart failed: {respawnError}</div>
        )}

        <div className="mt-4 text-xs text-text-subtle">
          The session is paused until the adapter satisfies the required version. After installing, use{" "}
          <span className="font-medium text-text-subtle">Restart agent</span> above (no full restart of{" "}
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
    <dl className="mt-4 grid grid-cols-[max-content_1fr] gap-x-3 gap-y-1 font-mono text-[11px] text-text-subtle">
      {rows.map(([label, value]) => (
        <div key={label} className="contents">
          <dt className="font-medium text-text-subtle">{label}</dt>
          <dd className="break-all">{value}</dd>
        </div>
      ))}
    </dl>
  );
}
