import { useEffect, useId, useState } from "react";

import type { IncompatibleAgentDetail } from "../../lib/acpTypes";
import { fetchAgentInstallInfo, type AgentInstallInfo } from "../../lib/api";
import { useRespawnSession } from "../../hooks/useRespawnSession";
import { useAdapterInstall } from "./adapterRecovery";
import { CommandBlock, RecoveryActionRow, type RecoveryAction } from "./RecoveryActionRow";

interface Props {
  detail: IncompatibleAgentDetail;
  sessionId: string;
  /** Used until install-info answers. */
  isSandboxed?: boolean;
}

const SETTING_PATH = 'TUI › Settings › Global › Acp › Advanced › "Allow agent install from web"';

function Code({ children }: { children: React.ReactNode }) {
  return <code className="rounded bg-surface-950 px-1 font-mono text-[12px]">{children}</code>;
}

/** install-info, re-read when the page regains focus so a setting turned on in
 *  the TUI unlocks the install without a reload. */
function useInstallInfo(sessionId: string): AgentInstallInfo | null {
  const [info, setInfo] = useState<AgentInstallInfo | null>(null);
  useEffect(() => {
    let alive = true;
    const load = () => {
      void fetchAgentInstallInfo(sessionId).then((next) => {
        if (alive && next) setInfo(next);
      });
    };
    const onVisible = () => {
      if (document.visibilityState === "visible") load();
    };
    load();
    window.addEventListener("focus", load);
    document.addEventListener("visibilitychange", onVisible);
    return () => {
      alive = false;
      window.removeEventListener("focus", load);
      document.removeEventListener("visibilitychange", onVisible);
    };
  }, [sessionId]);
  return info;
}

/** Replaces the chat when the adapter compatibility check refuses the Session:
 *  the steps that fix it from here, or the command to run by hand. */
export function StartupErrorScreen({ detail, sessionId, isSandboxed = false }: Props) {
  const info = useInstallInfo(sessionId);
  const sandboxed = info?.sandboxed ?? isSandboxed;
  const name = info?.display_name ?? null;
  const stepOneId = useId();
  const [detailsOpen, setDetailsOpen] = useState(false);

  const { state: respawnState, error: respawnError, respawn } = useRespawnSession(sessionId);
  // A version failure that arrives after a successful install is a new detail.
  const [installedFor, setInstalledFor] = useState<IncompatibleAgentDetail | null>(null);
  const install = useAdapterInstall(sessionId, () => {
    setInstalledFor(detail);
    return respawn();
  });
  const failsAgain = installedFor !== null && installedFor !== detail;
  // Cleared by the next detail, which arrives once the started container runs the adapter again.
  const [containerDownFor, setContainerDownFor] = useState<IncompatibleAgentDetail | null>(null);

  const hostCommand = "install_command" in detail ? detail.install_command : null;
  const manualCommand = sandboxed ? (info?.container_install_command ?? null) : hostCommand;
  // A sandboxed adapter npm can't install has no Daemon-built command; its hint runs in the container.
  const containerHint = sandboxed && !manualCommand ? hostCommand : null;
  // Read-only and CityHall dashboards can neither install nor respawn.
  const observer = info?.install_blocked === "read_only" || info?.install_blocked === "cityhall";
  const webInstall = !observer && "auto_install" in detail && detail.auto_install;
  const allowed = info?.install_allowed === true;
  const containerDown = containerDownFor === detail && install.errorCode === "container_not_running";
  const busy = respawnState === "retrying" || install.state === "installing";

  const updateLabel = sandboxed ? "Update in Sandbox & restart" : "Update & restart";
  const checkAgain: RecoveryAction = {
    label: respawnState === "retrying" ? "Checking…" : "Check again",
    onClick: () => void respawn(),
    testId: "startup-error-restart",
    disabled: busy,
  };
  const update: RecoveryAction = {
    label: install.state === "installing" ? "Updating…" : updateLabel,
    onClick: () =>
      void install.install().then((code) => {
        if (code === "container_not_running") setContainerDownFor(detail);
      }),
    testId: "startup-error-update-restart",
    disabled: busy || !allowed,
    describedBy: allowed ? undefined : stepOneId,
  };
  const startContainer: RecoveryAction = {
    label: respawnState === "retrying" ? "Starting…" : "Start the container",
    onClick: () => void respawn(),
    testId: "startup-error-start-container",
    disabled: busy,
  };
  const primary = containerDown ? startContainer : webInstall ? update : checkAgain;
  const secondary = containerDown ? [] : webInstall ? [checkAgain] : [];

  return (
    <div
      role="alert"
      aria-live="assertive"
      data-testid="startup-error-screen"
      className="flex h-full flex-1 items-center justify-center bg-surface-900 px-4 py-12"
    >
      <div className="w-full max-w-xl rounded-lg border border-status-error/60 bg-surface-850 p-4 text-text-primary shadow-lg sm:p-6">
        <div className="text-[11px] font-semibold uppercase tracking-wide text-status-error">
          {detail.kind === "unsupported_protocol_version" ? "Adapter not supported" : "Adapter out of date"}
        </div>
        <h2 className="mt-2 text-lg font-semibold text-text-primary">{headlineFor(detail, name)}</h2>
        <p data-testid="startup-error-version" className="mt-1 break-words font-mono text-[12px] text-text-secondary">
          {versionLineFor(detail)}
        </p>

        {failsAgain && (
          <p data-testid="startup-error-fails-again" className="mt-3 text-sm text-status-error">
            The update finished, but the adapter still fails the check, so the install didn't change the version it
            reports. Run the command below by hand, then Check again.
          </p>
        )}
        {containerDown && (
          <p data-testid="startup-error-container-down" className="mt-3 text-sm text-status-error">
            This Session's container isn't running, so the update had nowhere to run. Start the container, then update
            again.
          </p>
        )}

        <ol className="mt-4 space-y-3 text-sm" data-testid="startup-error-steps">
          {detail.kind === "unsupported_protocol_version" ? (
            <>
              <Step n={1} title={`Install an adapter that speaks ACP protocol ${detail.expected}`}>
                This aoe build negotiates protocol {detail.expected}; the adapter speaks {detail.received}. Install an
                adapter version for protocol {detail.expected}, or update aoe.
              </Step>
              {!observer && <Step n={2} title="Click Check again" />}
            </>
          ) : containerDown ? (
            <>
              <Step n={1} title="Click Start the container" />
              <Step n={2} title="Then update the adapter">
                The update comes back here once the container is running.
              </Step>
            </>
          ) : webInstall ? (
            <>
              <Step n={1} id={stepOneId} done={allowed} title="Allow installs from the Web dashboard">
                Turn on {SETTING_PATH}. The install runs <Code>npm install</Code> as the Daemon's user.
              </Step>
              <Step n={2} title={`Click ${updateLabel}`}>
                It updates the adapter {sandboxed ? "in this Session's container" : "where the Daemon runs"} and
                restarts the agent.
                {manualCommand && (
                  <div className="mt-2" data-testid="startup-error-fallback">
                    <p className="text-xs text-text-dim">Or run this where the Daemon runs, then Check again:</p>
                    <CommandBlock command={manualCommand} testId="startup-error-install-command" scroll />
                  </div>
                )}
              </Step>
            </>
          ) : (
            <>
              <Step
                n={1}
                title={containerHint ? "Run this inside this Session's container" : "Run this where the Daemon runs"}
              >
                {manualCommand || containerHint ? (
                  <CommandBlock
                    command={(manualCommand ?? containerHint)!}
                    testId="startup-error-install-command"
                    scroll
                  />
                ) : (
                  <>
                    Run <Code>aoe acp doctor --fix</Code>.
                  </>
                )}
              </Step>
              <Step n={2} title={observer ? "Restart the agent from the TUI" : "Click Check again"}>
                {observer &&
                  (info?.install_blocked === "cityhall"
                    ? "A CityHall dashboard can't restart the agent."
                    : "This dashboard is read-only, so it can't restart the agent.")}
              </Step>
            </>
          )}
        </ol>

        {!observer && (
          <RecoveryActionRow
            primary={primary}
            secondary={secondary}
            install={{ error: containerDown ? null : install.error, output: install.output }}
          />
        )}

        {respawnState === "ok" && (
          <div className="mt-2 text-xs text-status-running">
            Checking again. The adapter's version is checked when it starts.
            {install.recovered > 0 &&
              ` Also restarting ${install.recovered} other Session${install.recovered === 1 ? "" : "s"} blocked on the same adapter.`}
          </div>
        )}
        {respawnState === "failed" && respawnError && (
          <div className="mt-2 text-xs text-status-error">Check again failed: {respawnError}</div>
        )}

        <div className="mt-4 border-t border-status-error/30 pt-2">
          <button
            type="button"
            data-testid="startup-error-details"
            aria-expanded={detailsOpen}
            onClick={() => setDetailsOpen(!detailsOpen)}
            className="text-xs font-medium text-text-primary underline-offset-2 hover:underline"
          >
            Details
          </button>
          {detailsOpen && (
            <div data-testid="startup-error-details-body" className="mt-2 space-y-2 text-xs text-text-secondary">
              <p>{rationaleFor(detail)}</p>
              {sandboxed && (
                <p>
                  Refreshing the Sandbox image fixes new Sessions only: this Session's container keeps the image it
                  started with. The TUI's "sandbox image update available" banner pulls it with your Container runtime.
                </p>
              )}
              <p>
                Check again restarts the agent, and the check runs again at the next ACP <Code>initialize</Code>{" "}
                handshake. <Code>aoe serve</Code> doesn't need a restart.
              </p>
              <DetailRows detail={detail} sandboxed={sandboxed} />
            </div>
          )}
        </div>
      </div>
    </div>
  );
}

function Step({
  n,
  id,
  done = false,
  title,
  children,
}: {
  n: number;
  id?: string;
  done?: boolean;
  title: string;
  children?: React.ReactNode;
}) {
  return (
    <li id={id} className="flex gap-3" data-testid={`startup-error-step-${n}`} data-done={done || undefined}>
      <span
        aria-hidden
        className={`flex h-6 w-6 shrink-0 items-center justify-center rounded-full text-xs font-semibold ${
          done ? "bg-status-running/20 text-status-running" : "bg-surface-800 text-text-secondary"
        }`}
      >
        {done ? "✓" : n}
      </span>
      <div className="min-w-0 flex-1">
        <div className="font-medium text-text-primary">
          {title}
          {done && <span className="ml-2 text-xs font-normal text-status-running">Done</span>}
        </div>
        {children && <div className="mt-0.5 text-xs text-text-secondary">{children}</div>}
      </div>
    </li>
  );
}

function headlineFor(detail: IncompatibleAgentDetail, name: string | null): string {
  const adapter = name ? `The ${name} ACP adapter` : "The ACP adapter";
  switch (detail.kind) {
    case "incompatible_agent_version":
      return `${adapter} needs an update`;
    case "missing_agent_info":
      return `${adapter} didn't report its version`;
    case "mismatched_agent_name":
      return `${adapter} isn't the package aoe expects`;
    case "unparseable_agent_version":
      return `${adapter} reported a version aoe can't read`;
    case "unsupported_protocol_version":
      return `${adapter} speaks an ACP protocol aoe doesn't support`;
  }
}

function versionLineFor(detail: IncompatibleAgentDetail): string {
  switch (detail.kind) {
    case "incompatible_agent_version":
      return `${detail.package_name} ${detail.installed} installed · needs ${detail.required} or newer`;
    case "missing_agent_info":
      return `${detail.expected_package} · no version reported`;
    case "mismatched_agent_name":
      return `${detail.received} reported · expected ${detail.expected}`;
    case "unparseable_agent_version":
      return `${detail.package_name} "${detail.raw_version}" · needs ${detail.required} or newer`;
    case "unsupported_protocol_version":
      return `protocol ${detail.received} · needs ${detail.expected}`;
  }
}

function rationaleFor(detail: IncompatibleAgentDetail): string {
  switch (detail.kind) {
    case "incompatible_agent_version":
      return `aoe relies on behavior that ${detail.package_name} versions before ${detail.required} don't have, such as memory_recall tool calls and the native cancelled stop reason.`;
    case "missing_agent_info":
      return `The adapter returned no package version in its ACP initialize response. That usually means a stale install or a wrapper that strips the metadata.`;
    case "mismatched_agent_name":
      return `The adapter identified itself as ${detail.received}. That usually means a wrapper script or a stale binary is found first on PATH.`;
    case "unparseable_agent_version":
      return `The adapter reported "${detail.raw_version}", which isn't a semver version. Reinstalling the official build fixes it.`;
    case "unsupported_protocol_version":
      return `The adapter is from an older or newer generation than this aoe build supports.`;
  }
}

function DetailRows({ detail, sandboxed }: { detail: IncompatibleAgentDetail; sandboxed: boolean }) {
  const rows: Array<[string, string]> = [];
  switch (detail.kind) {
    case "incompatible_agent_version":
      rows.push(
        ["Package", detail.package_name],
        ["Installed", detail.installed],
        ["Required", `>=${detail.required}`],
      );
      break;
    case "missing_agent_info":
      rows.push(["Expected package", detail.expected_package]);
      break;
    case "mismatched_agent_name":
      rows.push(["Expected", detail.expected], ["Received", detail.received]);
      break;
    case "unparseable_agent_version":
      rows.push(
        ["Package", detail.package_name],
        ["Raw version", detail.raw_version],
        ["Required", `>=${detail.required}`],
      );
      break;
    case "unsupported_protocol_version":
      rows.push(["Expected protocol", detail.expected], ["Received protocol", detail.received]);
      break;
  }
  rows.push(["Where", sandboxed ? "This Session's container" : "Host, where the Daemon runs"]);
  return (
    <dl className="grid grid-cols-[max-content_1fr] gap-x-3 gap-y-1 font-mono text-[11px] text-text-secondary">
      {rows.map(([label, value]) => (
        <div key={label} className="contents">
          <dt className="font-medium text-text-dim">{label}</dt>
          <dd className="break-all">{value}</dd>
        </div>
      ))}
    </dl>
  );
}
