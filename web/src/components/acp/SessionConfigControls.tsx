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
import { useEffect, useLayoutEffect, useRef, useState } from "react";

import type { ConfigOptionDescriptor, AcpState } from "../../lib/acpTypes";

interface Props {
  configOptions: AcpState["configOptions"];
  pendingConfigOption: AcpState["pendingConfigOption"];
  onSetConfigOption: (configId: string, value: string) => void | Promise<void>;
}

const MODEL_LABEL_MAX = 24;
// Cap and floor for the menu's dynamically computed max-height (px). The
// cap keeps a short list from looking absurdly tall. The floor is only a
// threshold for picking which direction the menu opens in (see
// `computeMenuLayout`): rendered height is always clamped to the actual
// space available in the chosen direction, so the menu never extends
// past the viewport regardless of which side has room.
const MENU_MAX_HEIGHT_CAP = 288;
const MENU_MAX_HEIGHT_FLOOR = 120;
// Buffer kept beyond the trigger's own `mb-1`/`mt-1` (4px) gap so the
// menu's outer edge never touches the viewport edge, in whichever
// direction it opens.
const MENU_VIEWPORT_MARGIN = 8;
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

export function SessionConfigControls({ configOptions, pendingConfigOption, onSetConfigOption }: Props) {
  const model = findByCategory(configOptions, "model");
  const effort = findByCategory(configOptions, "thought_level");

  // Hidden entirely when neither selector exists; avoids empty chrome
  // on adapters that don't advertise either category.
  if (!model && !effort) return null;

  return (
    <div data-testid="session-config-controls" className="flex flex-wrap items-center gap-1.5">
      {model && (
        <ModelDropdown
          option={model}
          pending={pendingConfigOption?.configId === model.id ? pendingConfigOption.value : null}
          onSelect={(value) => onSetConfigOption(model.id, value)}
        />
      )}
      {effort && (
        <EffortControl
          option={effort}
          pending={pendingConfigOption?.configId === effort.id ? pendingConfigOption.value : null}
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

interface MenuLayout {
  direction: "up" | "down";
  maxHeight: number;
}

const DEFAULT_MENU_LAYOUT: MenuLayout = { direction: "up", maxHeight: MENU_MAX_HEIGHT_CAP };

/** Picks which side of the trigger the menu opens toward and how tall it
 *  may render, from the trigger's actual position in the viewport.
 *  Prefers opening upward (this menu's usual position, anchored above a
 *  footer control) as long as there's at least floor-height room there;
 *  otherwise flips to whichever side has more room. Height is always
 *  clamped to the space actually available in the chosen direction, so
 *  the menu can render shorter than the floor on a very cramped
 *  viewport, but it never extends past the viewport edge.
 *
 *  `viewportHeight` should be `window.visualViewport?.height ??
 *  window.innerHeight`: on iOS Safari, `innerHeight` stays at the full
 *  layout height while the software keyboard is raised, so it alone
 *  would report room below the trigger that the keyboard has actually
 *  covered. `viewportTop` should be `window.visualViewport?.offsetTop ??
 *  0`: `getBoundingClientRect()` is relative to the layout viewport, but
 *  the visible vertical interval is `offsetTop .. offsetTop + height`
 *  (CSSOM View); a non-zero offset (e.g. after pinch-zoom) shifts both
 *  edges of that interval, not just its size. */
function computeMenuLayout(rect: DOMRect, viewportHeight: number, viewportTop = 0): MenuLayout {
  const spaceAbove = rect.top - viewportTop - MENU_VIEWPORT_MARGIN;
  const spaceBelow = viewportTop + viewportHeight - rect.bottom - MENU_VIEWPORT_MARGIN;
  let direction: "up" | "down";
  let available: number;
  if (spaceAbove >= MENU_MAX_HEIGHT_FLOOR) {
    direction = "up";
    available = spaceAbove;
  } else if (spaceBelow >= MENU_MAX_HEIGHT_FLOOR || spaceBelow > spaceAbove) {
    direction = "down";
    available = spaceBelow;
  } else {
    direction = "up";
    available = spaceAbove;
  }
  return { direction, maxHeight: Math.max(0, Math.min(MENU_MAX_HEIGHT_CAP, available)) };
}

function ModelDropdown({ option, pending, onSelect }: SubProps) {
  const [open, setOpen] = useState(false);
  const [menuLayout, setMenuLayout] = useState<MenuLayout>(DEFAULT_MENU_LAYOUT);
  const ref = useRef<HTMLDivElement | null>(null);
  const menuId = `config-option-menu-${option.id}`;
  const current = option.options.find((o) => o.value === option.current_value) ?? option.options[0];
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

  // The menu prefers opening upward, so its usual ceiling is the trigger
  // button's distance from the top of the viewport, not a fixed guess.
  // Recomputed on open and on resize/scroll so a short viewport (or one
  // that shrinks after opening) still leaves it fully on-screen, flipping
  // to open downward when there isn't enough room above. Also listens on
  // `visualViewport` so a software keyboard raising/lowering while the
  // menu is open (which does not fire `window`'s `resize`) still
  // recomputes. See `computeMenuLayout`.
  useLayoutEffect(() => {
    if (!open) return;
    const vv = typeof window !== "undefined" ? window.visualViewport : null;
    const recompute = () => {
      const rect = ref.current?.getBoundingClientRect();
      if (!rect) return;
      setMenuLayout(computeMenuLayout(rect, vv?.height ?? window.innerHeight, vv?.offsetTop ?? 0));
    };
    recompute();
    window.addEventListener("resize", recompute);
    window.addEventListener("scroll", recompute, true);
    vv?.addEventListener("resize", recompute);
    vv?.addEventListener("scroll", recompute);
    return () => {
      window.removeEventListener("resize", recompute);
      window.removeEventListener("scroll", recompute, true);
      vv?.removeEventListener("resize", recompute);
      vv?.removeEventListener("scroll", recompute);
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
          className={[
            "absolute left-0 z-30 flex w-64 flex-col overflow-hidden rounded-md border border-surface-700 bg-surface-850 shadow-xl",
            menuLayout.direction === "up" ? "bottom-full mb-1" : "top-full mt-1",
          ].join(" ")}
          style={{ maxHeight: menuLayout.maxHeight }}
          role="menu"
        >
          <div className="border-b border-surface-800 px-3 py-1.5 text-[10px] uppercase tracking-wider text-text-dim">
            {option.name}
          </div>
          <div className="overflow-y-auto">
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
                    {opt.description && <span className="block text-[11px] text-text-dim">{opt.description}</span>}
                  </span>
                  {isCurrent && !isPending && <span className="text-[10px] uppercase text-brand-500">Active</span>}
                  {isPending && <span className="text-[10px] uppercase text-text-dim">…</span>}
                </button>
              );
            })}
          </div>
        </div>
      )}
    </div>
  );
}

function EffortControl(props: SubProps) {
  const { option } = props;
  const totalLabelLen = option.options.reduce((acc, o) => acc + o.name.length, 0);
  const useSegmented =
    option.options.length > 0 &&
    option.options.length <= EFFORT_SEGMENTED_MAX_COUNT &&
    totalLabelLen <= EFFORT_SEGMENTED_MAX_TOTAL_LABEL_LEN;
  return useSegmented ? <EffortSegmented {...props} /> : <ModelDropdown {...props} />;
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
              isCurrent ? "bg-surface-700 text-text-primary" : "text-text-secondary hover:text-text-primary",
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
export function ConfigOptionSwitchFailedNotice({ failure, configOptions, onDismiss }: NoticeProps) {
  if (!failure) return null;
  const config = configOptions.find((c) => c.id === failure.configId);
  const optionLabel = config?.options.find((o) => o.value === failure.value)?.name ?? failure.value;
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
