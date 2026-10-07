import { useState } from "react";
import { ChevronRight } from "lucide-react";
import type { SystemHealth } from "../lib/api";
import { useSystemHealth } from "../hooks/useSystemHealth";
import { useSidebarCompact } from "../lib/sidebarCompact";
import { formatBytes, formatPercent, useSystemHealthEnabled } from "../lib/systemHealth";

/** Band colors mirror the TUI strip: calm reads as running green, warn as the
 *  waiting amber, critical as the error red. */
const STATUS_COLOR: Record<SystemHealth["status"], string> = {
  ok: "text-status-running",
  warn: "text-status-warning",
  critical: "text-status-error",
};

function DetailRow({ label, value }: { label: string; value: string }) {
  return (
    <div className="flex gap-2">
      <span className="w-14 shrink-0 text-text-secondary">{label}</span>
      <span className="tabular-nums">{value}</span>
    </div>
  );
}

/** The sidebar footer strip, gated on the same "Show system health strip"
 *  setting as the TUI's. Renders and polls nothing while the setting is off. */
export function SidebarSystemHealth() {
  const enabled = useSystemHealthEnabled();
  // The compact rail is 88px wide, which the readout cannot fit without
  // wrapping, so the strip stands down there rather than shipping a second
  // cramped layout of the same numbers.
  const compact = useSidebarCompact();
  // The disclosure state sets the poll rate, so it lives with the poll rather
  // than inside the strip: only an open drill-down needs a fast refresh.
  const [expanded, setExpanded] = useState(false);
  const health = useSystemHealth(enabled && !compact, expanded);
  if (!enabled || compact) return null;
  return <SystemHealthStrip health={health} expanded={expanded} onToggle={() => setExpanded((v) => !v)} />;
}

/** Compact host-health readout below the session list, with the per-agent
 *  table behind a disclosure. Mirrors the TUI's strip and its drill-down. */
export function SystemHealthStrip({
  health,
  expanded,
  onToggle,
}: {
  health: SystemHealth | null;
  expanded: boolean;
  onToggle: () => void;
}) {
  if (!health) return null;

  const memoryKnown = health.memory_total_bytes > 0;
  const memory = memoryKnown ? formatPercent(health.memory_used_bytes / health.memory_total_bytes) : "?";
  const bandColor = STATUS_COLOR[health.status];
  const load = health.load_average ? health.load_average.map((v) => v.toFixed(2)).join(" / ") : "? / ? / ?";
  const swap =
    health.swap_total_bytes > 0
      ? `${formatBytes(health.swap_used_bytes)} / ${formatBytes(health.swap_total_bytes)}`
      : "none";

  return (
    <div className="border-t border-surface-700/20 text-[11px]" data-testid="system-health-strip">
      <button
        onClick={() => onToggle()}
        aria-expanded={expanded}
        title="System health"
        className="w-full flex items-center gap-1.5 px-2 py-1.5 hover:bg-surface-800/50 cursor-pointer transition-colors"
      >
        <span className="font-medium tabular-nums">CPU {formatPercent(health.cpu_fraction)}</span>
        <span className="text-text-secondary">·</span>
        <span className={`font-medium tabular-nums ${bandColor}`}>Mem {memory}</span>
        <span className="ml-auto text-text-secondary tabular-nums">
          {health.agent_count} agents · {health.proc_count} procs
        </span>
        <ChevronRight
          size={12}
          className={`shrink-0 text-text-secondary transition-transform ${expanded ? "rotate-90" : ""}`}
        />
      </button>

      {expanded && (
        <div className="px-2 pb-2 space-y-2" data-testid="system-health-detail">
          <div className="space-y-0.5">
            <div className="flex gap-2">
              <span className="w-14 shrink-0 text-text-secondary">Status</span>
              {/* The server names the band; upper-casing it here keeps the
                  status row reading like the TUI's without a second table. */}
              <span className={`font-medium ${bandColor}`}>{health.status.toUpperCase()}</span>
            </div>
            <DetailRow
              label="Memory"
              value={
                memoryKnown
                  ? `${memory}   ${formatBytes(health.memory_used_bytes)} / ${formatBytes(health.memory_total_bytes)}`
                  : "?"
              }
            />
            <DetailRow label="Load" value={load} />
            <DetailRow label="Swap" value={swap} />
          </div>

          {health.agents.length === 0 ? (
            <div className="text-text-secondary">No running AoE agents</div>
          ) : (
            <table className="w-full tabular-nums">
              <thead className="text-text-secondary">
                <tr>
                  <th className="text-left font-medium">Agent</th>
                  {/* "Mem", not "RSS": a sandboxed row reports the container's
                      memory usage, which is not a resident-set sum. */}
                  <th className="text-right font-medium">CPU</th>
                  <th className="text-right font-medium">Mem</th>
                  <th className="text-right font-medium">Procs</th>
                </tr>
              </thead>
              <tbody>
                {health.agents.map((agent) => (
                  <tr key={agent.id}>
                    <td className="truncate max-w-0 w-full pr-2">
                      {agent.title}
                      {agent.sandboxed && <span className="text-text-secondary"> [container]</span>}
                    </td>
                    <td className="text-right">{formatPercent(agent.cpu_fraction)}</td>
                    <td className="text-right">{agent.memory_bytes == null ? "?" : formatBytes(agent.memory_bytes)}</td>
                    <td className="text-right">{agent.procs ?? "?"}</td>
                  </tr>
                ))}
              </tbody>
            </table>
          )}
        </div>
      )}
    </div>
  );
}
