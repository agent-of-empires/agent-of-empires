import { useEffect, useState } from "react";

import {
  fetchTelemetryStatus,
  setTelemetryConsent,
  type TelemetryStatus,
} from "../../lib/api";
import { ToggleField } from "./FormFields";

/// Telemetry opt-in toggle. Unlike the other settings panels this does not go
/// through the generic settings PATCH: the daemon owns the anonymous install
/// id (the browser never posts to the telemetry backend), so the toggle calls
/// the dedicated consent endpoint, which also generates / deletes the id.
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
        // fetchTelemetryStatus already swallows network errors and returns
        // null, but guard here too so a throw can never leave the panel blank.
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
      // Always clear the saving flag so the toggle can't get stuck disabled.
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
          DO_NOT_TRACK is set in the server environment, so telemetry stays off
          and no install id is generated regardless of this toggle.
        </p>
      )}
    </div>
  );
}
