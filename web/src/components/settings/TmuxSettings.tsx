import { NumberField, SelectField } from "./FormFields";

const DEFAULT_TMUX_HISTORY_LIMIT = 2000;
const MAX_TMUX_HISTORY_LIMIT = 200000;

function readHistoryLimit(value: unknown): number {
  return typeof value === "number" && Number.isFinite(value)
    ? value
    : DEFAULT_TMUX_HISTORY_LIMIT;
}

function normalizeHistoryLimit(value: number): number {
  return Math.max(0, Math.min(MAX_TMUX_HISTORY_LIMIT, Math.floor(value)));
}

interface Props {
  settings: Record<string, unknown>;
  onSaveField: (section: string, field: string, value: unknown) => void;
  onUpdate: (patch: Record<string, unknown>) => void;
}

export function TmuxSettings({ settings, onSaveField, onUpdate }: Props) {
  const tmux = (settings.tmux ?? {}) as Record<string, unknown>;

  const save = (field: string, value: unknown) => {
    onUpdate({ tmux: { ...tmux, [field]: value } });
    onSaveField("tmux", field, value);
  };

  const modeOptions = [
    { value: "auto", label: "Auto" },
    { value: "enabled", label: "Enabled" },
    { value: "disabled", label: "Disabled" },
  ];

  return (
    <div className="space-y-4">
      <p className="text-xs text-text-dim">
        These settings apply to the TUI (local tmux sessions), not the web dashboard.
        tmux itself does not control font size; web session font size is in
        Terminal settings, and local TUI font size comes from your terminal app.
      </p>
      <SelectField
        label="Status bar"
        description="Show tmux status bar in sessions"
        value={(tmux.status_bar as string) ?? "auto"}
        onChange={(v) => save("status_bar", v)}
        options={modeOptions}
      />
      <SelectField
        label="Mouse support"
        description="Enable mouse in tmux sessions"
        value={(tmux.mouse as string) ?? "auto"}
        onChange={(v) => save("mouse", v)}
        options={modeOptions}
      />
      <NumberField
        label="History limit"
        description="Scrollback lines kept by tmux for each AoE session"
        value={readHistoryLimit(tmux.history_limit)}
        min={0}
        max={MAX_TMUX_HISTORY_LIMIT}
        onChange={(v) => save("history_limit", normalizeHistoryLimit(v))}
      />
    </div>
  );
}
