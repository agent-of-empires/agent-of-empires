// Kept out of `api.ts` so the test setup can stub every ping without touching API mocks.
// Accessory dashboard features the daemon counts on `/api/telemetry/feature-use`.
// Mirrors the registry in `src/telemetry/feature_use.rs`; off-list names get a 400.
export type FeatureUse =
  | "sort_manual"
  | "sort_last_activity"
  | "sort_attention"
  | "sort_plugin"
  | "axis_repo"
  | "axis_org"
  | "axis_group"
  | "axis_repo_group"
  | "axis_status"
  | "pin_toggle"
  | "archive_toggle"
  | "snooze_toggle"
  | "unread_toggle"
  | "row_reorder"
  | "attention_jump"
  | "session_color"
  | "focus_mode"
  | "fork"
  | "auto_name"
  | "summarize"
  | "files_pane"
  | "agents_pane"
  | "cheat_code";

// Count one use of an accessory feature for the daemon's next opt-in snapshot. Best-effort.
export function reportFeatureUse(feature: FeatureUse): void {
  void fetch("/api/telemetry/feature-use", {
    method: "POST",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify({ feature }),
    keepalive: true,
  }).catch(() => {});
}
