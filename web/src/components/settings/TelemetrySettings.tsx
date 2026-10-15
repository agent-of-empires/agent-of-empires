import { useEffect, useState } from "react";

import { fetchTelemetryStatus, setTelemetryConsent, type TelemetryStatus } from "../../lib/api";
import { ToggleField } from "./FormFields";

/// Uses the dedicated consent endpoint, which also creates or deletes the install id.
export function TelemetrySettings() {
  const [status, setStatus] = useState<TelemetryStatus | null>(null);
  const [saving, setSaving] = useState(false);

  useEffect(() => {
    let active = true;
    void (async () => {
      try {
        const s = await fetchTelemetryStatus();
        if (active) setStatus(s);
      } catch {
        // Keep the panel rendered even if the fetch throws.
      }
    })();
    return () => {
      active = false;
    };
  }, []);

  const onToggle = async (enabled: boolean) => {
    setSaving(true);
    try {
      const next = await setTelemetryConsent(enabled);
      if (next) setStatus(next);
    } finally {
      setSaving(false);
    }
  };

  const dnt = status?.do_not_track ?? false;
  const enabled = status?.enabled ?? false;

  return (
    <div className="space-y-4">
      <ToggleField
        label="Enable usage telemetry"
        description="Anonymous, opt-in usage telemetry: counts of sessions, agents/models, your aoe version, and OS. Off by default. Never sends prompts, paths, names, branches, or commands. Honors DO_NOT_TRACK."
        checked={enabled && !dnt}
        onChange={(v) => {
          if (!dnt && !saving) void onToggle(v);
        }}
      />
      {dnt && (
        <p className="text-xs text-text-dim">
          DO_NOT_TRACK is set in the server environment, so telemetry stays off and no install id is generated
          regardless of this toggle.
        </p>
      )}
    </div>
  );
}
