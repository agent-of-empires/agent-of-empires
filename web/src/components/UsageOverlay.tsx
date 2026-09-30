import { useEffect, useState } from "react";
import type { SessionResponse } from "../lib/types";
import { fetchSessionUsage, usageLines, useUsageOverlayEnabled, type UsageSummary } from "../lib/usage";

const POLL_MS = 5_000;

/** Wraps the impure `Date.now()` read outside the component body, matching
 *  `ConnectedDevices`' `relativeTime` helper. */
function currentUsageLines(summary: UsageSummary, createdAt: string): string[] {
  return usageLines(summary, createdAt, Date.now());
}

/** The session's context-reset count, big and red, over the terminal's top-right corner. */
export function UsageOverlay({ session }: { session: SessionResponse }) {
  const enabled = useUsageOverlayEnabled();
  const [summary, setSummary] = useState<UsageSummary | null>(null);

  useEffect(() => {
    if (!enabled) return;
    let cancelled = false;
    const load = () => {
      if (document.visibilityState !== "visible") return;
      void fetchSessionUsage(session.id).then((next) => {
        if (!cancelled) setSummary(next);
      });
    };
    load();
    const timer = window.setInterval(load, POLL_MS);
    return () => {
      cancelled = true;
      window.clearInterval(timer);
    };
  }, [enabled, session.id]);

  if (!enabled || !summary?.tracked) return null;
  return (
    <div
      className="pointer-events-none absolute top-2 right-3 z-10 hidden rounded-md bg-surface-900/25 px-2 py-1 text-right sm:block"
      data-testid="usage-overlay"
      title="Context resets: clears + compactions + resumes"
    >
      <div className="text-3xl font-black leading-none text-status-error tabular-nums opacity-90">{summary.resets}</div>
      {currentUsageLines(summary, session.created_at).map((line) => (
        <div key={line} className="mt-1 font-mono text-[10px] text-text-secondary opacity-90">
          {line}
        </div>
      ))}
    </div>
  );
}
