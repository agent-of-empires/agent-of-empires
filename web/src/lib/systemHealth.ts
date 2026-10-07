import { sessionFlagGate } from "./sessionFlagGate";

/** The system-health strip is off by default, matching the server's
 *  `session.show_diagnostics_pane` default. */
const gate = sessionFlagGate("show_diagnostics_pane", false);

export const SystemHealthEnabledContext = gate.Context;
export const parseSystemHealthEnabled = gate.parse;
export const useSystemHealthEnabled = gate.use;

/** Compact binary-unit byte string: `22.7G`, `512M`, `1K`, `512B`. GiB keeps
 *  one decimal but drops a whole `.0`; smaller units round to a whole number.
 *  Mirrors the TUI strip's `format_bytes`, so a reading is spelled the same on
 *  both surfaces; both test suites pin the same case table. */
export function formatBytes(bytes: number): string {
  const KIB = 1024;
  const MIB = 1024 * KIB;
  const GIB = 1024 * MIB;

  if (bytes >= GIB) {
    const tenths = Math.round((bytes * 10) / GIB);
    return tenths % 10 === 0 ? `${tenths / 10}G` : `${(tenths / 10).toFixed(1)}G`;
  }
  if (bytes >= MIB) return `${Math.round(bytes / MIB)}M`;
  if (bytes >= KIB) return `${Math.round(bytes / KIB)}K`;
  return `${bytes}B`;
}

/** Percent readout for an optional fraction; unknown reads `?`, never `0%`. */
export function formatPercent(fraction: number | null | undefined): string {
  return fraction == null ? "?" : `${Math.round(fraction * 100)}%`;
}
