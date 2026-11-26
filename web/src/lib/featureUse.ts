// Kept out of `api.ts` so the test setup can stub every ping without touching API mocks.
// Accessory dashboard features the daemon counts on `/api/telemetry/feature-use`.
// Mirrors the registry in `src/telemetry/feature_use.rs`; off-list names get a 400.
export type FeatureUse =
  | "sort_custom"
  | "sort_newest"
  | "sort_az"
  | "sort_plugin"
  | "group_by_project"
  | "group_by_manual"
  | "pin_toggle"
  | "archive_toggle"
  | "snooze_toggle"
  | "unread_toggle"
  | "row_reorder"
  | "attention_jump"
  | "focus_mode"
  | "fork"
  | "auto_name"
  | "summarize"
  | "files_pane"
  | "agents_pane"
  | "cheat_code"
  | "suggest_tip_shown"
  | "suggest_tip_dismissed"
  | "suggest_tip_clicked"
  | "suggest_setup_opened"
  | "suggest_setup_finished"
  | "suggest_setup_later"
  | "suggest_start_another"
  | "suggest_from_home"
  | "suggest_from_page"
  | "suggest_from_sidebar";

// Count one use of an accessory feature for the daemon's next opt-in snapshot. Best-effort.
export function reportFeatureUse(feature: FeatureUse): void {
  void fetch("/api/telemetry/feature-use", {
    method: "POST",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify({ feature }),
    keepalive: true,
  }).catch(() => {});
}
