import type { IncompatibleAgentDetail } from "../../lib/acpTypes";

interface Props {
  detail: IncompatibleAgentDetail;
}

/** Dedicated full-region replacement for the structured view chat layout when
 *  the per-adapter compatibility check refuses the session. Distinct
 *  from `StartupErrorBanner`, which is a smaller text-based hint
 *  layered on top of the chat for free-form handshake failures. This
 *  screen surfaces the structured detail (installed vs required
 *  version, the exact remediation command) so the user can copy-paste
 *  it into a shell without parsing prose. */
export function StartupErrorScreen({ detail }: Props) {
  const heading = headingFor(detail);
  const summary = summaryFor(detail);
  const installCommand = installCommandFor(detail);

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
        <h2 className="mt-2 text-lg font-semibold text-text-primary">
          {heading}
        </h2>
        <p className="mt-3 text-sm text-text-secondary">{summary}</p>

        {installCommand && (
          <div className="mt-4">
            <div className="text-[11px] font-medium uppercase tracking-wide text-text-dim">
              Run this, then restart the session
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

        <div className="mt-4 text-xs text-text-dim">
          The session is paused until the adapter satisfies the required
          version. Once you have run the command above, restart{" "}
          <code className="rounded bg-surface-950 px-1 font-mono text-[12px]">
            aoe serve
          </code>{" "}
          (or spawn a fresh structured view session) and the check re-runs at
          the next ACP{" "}
          <code className="rounded bg-surface-950 px-1 font-mono text-[12px]">
            initialize
          </code>{" "}
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
