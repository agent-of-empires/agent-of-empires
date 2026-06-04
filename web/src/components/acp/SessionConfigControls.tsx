// Structured view per-session config picker (#1403).
//
// Renders the model dropdown + reasoning-effort selector by filtering
// the unified `configOptions` snapshot the daemon publishes from
// ACP `SessionUpdate::ConfigOptionUpdate`. The mode picker lives in the
// composer (`ModePicker`); it reads a `category:"mode"` config option
// from this same `configOptions` snapshot when the agent advertises one
// (OpenCode, claude-agent-acp v0.37.0+), and only falls back to the ACP
// SessionModeState channel otherwise. See lib/modeChannel.ts (#1764).
//
// Behavior:
// - Pessimistic UI. Current value stays put until the adapter pushes a
//   confirming `ConfigOptionsUpdated`. The clicked option is dimmed
//   and disabled while `pendingConfigOption?.configId === id`.
// - Effort is adaptive: segmented control when the option count and
//   total label width comfortably fit, dropdown fallback otherwise.
//   The threshold is intentionally simple (count + label-length); a
//   container query is YAGNI until adapters actually emit long lists.
// - Hidden entirely when neither category appears.
// - `ConfigOptionSwitchFailedNotice` lives in this file because it
//   shares the dismiss callback's home.

import { ChevronUp } from "lucide-react";
import { useEffect, useRef, useState } from "react";

import type {
  ConfigOptionDescriptor,
  AcpState,
} from "../../lib/acpTypes";

interface Props {
  configOptions: AcpState["configOptions"];
  pendingConfigOption: AcpState["pendingConfigOption"];
  onSetConfigOption: (configId: string, value: string) => void | Promise<void>;
}

const MODEL_LABEL_MAX = 24;
const EFFORT_SEGMENTED_MAX_COUNT = 5;
const EFFORT_SEGMENTED_MAX_TOTAL_LABEL_LEN = 40;

function truncate(s: string, max: number): string {
  if (s.length <= max) return s;
  return s.slice(0, Math.max(0, max - 1)) + "…";
}

function findByCategory(
  options: ConfigOptionDescriptor[],
  category: "model" | "thought_level",
): ConfigOptionDescriptor | undefined {
  return options.find((o) => o.category === category);
}

export function SessionConfigControls({
  configOptions,
  pendingConfigOption,
  onSetConfigOption,
}: Props) {
  const model = findByCategory(configOptions, "model");
  const effort = findByCategory(configOptions, "thought_level");

  // Hidden entirely when neither selector exists; avoids empty chrome
  // on adapters that don't advertise either category.
  if (!model && !effort) return null;

  return (
    <div
      data-testid="session-config-controls"
      className="flex flex-wrap items-center gap-1.5"
    >
      {model && (
        <ModelDropdown
          option={model}
          pending={
            pendingConfigOption?.configId === model.id
              ? pendingConfigOption.value
              : null
          }
          onSelect={(value) => onSetConfigOption(model.id, value)}
        />
      )}
      {effort && (
        <EffortControl
          option={effort}
          pending={
            pendingConfigOption?.configId === effort.id
              ? pendingConfigOption.value
              : null
          }
          onSelect={(value) => onSetConfigOption(effort.id, value)}
        />
      )}
    </div>
  );
}

interface SubProps {
  option: ConfigOptionDescriptor;
  /** The value currently in flight for this option (rendered with a
   *  pending affordance), or null when nothing is pending. */
  pending: string | null;
  onSelect: (value: string) => void | Promise<void>;
}

function ModelDropdown({ option, pending, onSelect }: SubProps) {
  const [open, setOpen] = useState(false);
  const ref = useRef<HTMLDivElement | null>(null);
  const menuId = `config-option-menu-${option.id}`;
  const current =
    option.options.find((o) => o.value === option.current_value) ??
    option.options[0];
  const label = current?.name ?? option.current_value;

  useEffect(() => {
    if (!open) return;
    const onClick = (e: MouseEvent) => {
      if (!ref.current?.contains(e.target as Node)) setOpen(false);
    };
    const onKey = (e: KeyboardEvent) => {
      if (e.key === "Escape") setOpen(false);
    };
    document.addEventListener("mousedown", onClick);
    document.addEventListener("keydown", onKey);
    return () => {
      document.removeEventListener("mousedown", onClick);
      document.removeEventListener("keydown", onKey);
    };
  }, [open]);

  return (
    <div ref={ref} className="relative">
      <button
        type="button"
        onClick={() => setOpen((v) => !v)}
        aria-haspopup="menu"
        aria-expanded={open}
        aria-controls={open ? menuId : undefined}
        title={`${option.name}: ${label}`}
        aria-label={`${option.name}: ${label}`}
        data-testid={`config-option-${option.id}`}
        className={[
          "inline-flex items-center gap-1 rounded-md border border-surface-700 bg-surface-800/60 px-2 py-1 text-[11px] font-medium",
          "text-text-secondary",
          "transition-colors hover:border-brand-600/60 hover:text-text-primary",
        ].join(" ")}
      >
        <span>{truncate(label, MODEL_LABEL_MAX)}</span>
        <ChevronUp className="h-3 w-3 opacity-70" />
      </button>
      {open && (
        <div
          id={menuId}
          className="absolute bottom-full left-0 z-30 mb-1 w-64 overflow-hidden rounded-md border border-surface-700 bg-surface-850 shadow-xl"
          role="menu"
        >
          <div className="border-b border-surface-800 px-3 py-1.5 text-[10px] uppercase tracking-wider text-text-dim">
            {option.name}
          </div>
          {option.options.map((opt) => {
            const isCurrent = opt.value === option.current_value;
            const isPending = pending === opt.value;
            return (
              <button
                key={opt.value}
                type="button"
                role="menuitem"
                disabled={isPending}
                onClick={() => {
                  if (isPending || isCurrent) {
                    setOpen(false);
                    return;
                  }
                  setOpen(false);
                  void onSelect(opt.value);
                }}
                data-testid={`config-option-${option.id}-value-${opt.value}`}
                className={[
                  "flex w-full items-start gap-2 px-3 py-1.5 text-left text-[12px]",
                  isCurrent
                    ? "bg-surface-800 text-text-primary"
                    : "text-text-secondary hover:bg-surface-800 hover:text-text-primary",
                  isPending ? "cursor-not-allowed opacity-50" : "",
                ].join(" ")}
              >
                <span className="flex-1">
                  <span className="block font-medium">{opt.name}</span>
                  {opt.description && (
                    <span className="block text-[11px] text-text-dim">
                      {opt.description}
                    </span>
                  )}
                </span>
                {isCurrent && !isPending && (
                  <span className="text-[10px] uppercase text-brand-500">
                    Active
                  </span>
                )}
                {isPending && (
                  <span className="text-[10px] uppercase text-text-dim">
                    …
                  </span>
                )}
              </button>
            );
          })}
        </div>
      )}
    </div>
  );
}

function EffortControl(props: SubProps) {
  const { option } = props;
  const totalLabelLen = option.options.reduce(
    (acc, o) => acc + o.name.length,
    0,
  );
  const useSegmented =
    option.options.length > 0 &&
    option.options.length <= EFFORT_SEGMENTED_MAX_COUNT &&
    totalLabelLen <= EFFORT_SEGMENTED_MAX_TOTAL_LABEL_LEN;
  return useSegmented ? (
    <EffortSegmented {...props} />
  ) : (
    <ModelDropdown {...props} />
  );
}

function EffortSegmented({ option, pending, onSelect }: SubProps) {
  return (
    <div
      role="radiogroup"
      aria-label={option.name}
      data-testid={`config-option-${option.id}`}
      className="inline-flex items-center gap-0.5 rounded-md border border-surface-700 bg-surface-800/60 p-0.5"
    >
      {option.options.map((opt) => {
        const isCurrent = opt.value === option.current_value;
        const isPending = pending === opt.value;
        return (
          <button
            key={opt.value}
            type="button"
            role="radio"
            aria-checked={isCurrent}
            disabled={isPending}
            onClick={() => {
              if (isPending || isCurrent) return;
              void onSelect(opt.value);
            }}
            title={opt.description ?? `${option.name}: ${opt.name}`}
            data-testid={`config-option-${option.id}-value-${opt.value}`}
            className={[
              "rounded px-2 py-0.5 text-[11px] font-medium transition-colors",
              isCurrent
                ? "bg-surface-700 text-text-primary"
                : "text-text-secondary hover:text-text-primary",
              isPending ? "cursor-not-allowed opacity-50" : "",
            ].join(" ")}
          >
            {opt.name}
          </button>
        );
      })}
    </div>
  );
}

interface NoticeProps {
  failure: AcpState["configOptionSwitchFailed"];
  configOptions: AcpState["configOptions"];
  onDismiss: () => void;
}

/** Non-blocking notice rendered near the picker when the adapter
 *  rejects a `session/set_config_option`. Auto-dismisses via the
 *  reducer when a later snapshot confirms the requested value; the
 *  manual dismiss button is the user-visible escape hatch. */
export function ConfigOptionSwitchFailedNotice({
  failure,
  configOptions,
  onDismiss,
}: NoticeProps) {
  if (!failure) return null;
  const config = configOptions.find((c) => c.id === failure.configId);
  const optionLabel =
    config?.options.find((o) => o.value === failure.value)?.name ??
    failure.value;
  const configLabel = config?.name ?? failure.configId;
  return (
    <div
      data-testid="config-option-switch-failed-notice"
      role="status"
      className="flex items-start gap-3 rounded-md border border-amber-700/60 bg-amber-900/30 px-3 py-2 text-[12px] text-amber-100"
    >
      <div className="flex-1">
        <div className="font-medium">
          {configLabel} could not switch to {optionLabel}
        </div>
        <div className="text-[11px] text-amber-200/80">{failure.reason}</div>
      </div>
      <button
        type="button"
        onClick={onDismiss}
        aria-label="Dismiss notice"
        className="rounded px-1.5 py-0.5 text-amber-100 hover:bg-amber-700/30"
      >
        Dismiss
      </button>
    </div>
  );
}
