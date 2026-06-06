import { ChevronUp } from "lucide-react";
import { useCallback, useEffect, useMemo, useRef, useState } from "react";

import {
  agentBaseModelOptions,
  stripFastLabel,
  stripFastVariant,
  composeAgentModel,
  modelSupportsFast,
} from "../../lib/agentModelOptions";
import type { ConfigOptionDescriptor } from "../../lib/acpTypes";

interface Props {
  sessionId: string;
  currentAgent: string | null;
  sessionTool?: string | null;
  currentModel: string | null | undefined;
  modelConfigOption?: ConfigOptionDescriptor | null;
  fastConfigOption?: ConfigOptionDescriptor | null;
  onSetConfigOption?: (configId: string, value: string) => void | Promise<void>;
}

function normalizedModel(model: string | null | undefined): string {
  const trimmed = model?.trim() ?? "";
  return trimmed.length > 0 ? trimmed : "auto";
}

function parseCursorModelValue(value: string): {
  base: string;
  fast: boolean;
  hasFastParam: boolean;
  metadata: string | null;
} {
  const trimmed = value.trim();
  const match = trimmed.match(/^(.+?)\[(.*)\]$/);
  if (!match) {
    return {
      base: stripFastVariant(trimmed),
      fast: trimmed.endsWith("-fast"),
      hasFastParam: trimmed.endsWith("-fast"),
      metadata: null,
    };
  }
  const metadata = match[2] ?? "";
  const fastMatch = metadata.match(/(?:^|,)fast=(true|false)(?:,|$)/);
  return {
    base: match[1] ?? trimmed,
    fast: fastMatch?.[1] === "true",
    hasFastParam: fastMatch != null,
    metadata,
  };
}

function stripCursorMetadataLabel(label: string): string {
  return label.replace(/\[.*\]$/, "");
}

function uiBaseFromModelValue(value: string): string {
  const base = parseCursorModelValue(value).base;
  return base === "default" ? "auto" : base;
}

function optionMatches(
  option: { id: string; label: string },
  query: string,
  extraSearchText = "",
): boolean {
  const terms = query
    .trim()
    .toLowerCase()
    .split(/\s+/)
    .filter(Boolean);
  if (terms.length === 0) return true;
  const haystack = `${option.id} ${option.label} ${extraSearchText}`.toLowerCase();
  return terms.every((term) => haystack.includes(term));
}

function baseOptionsFromConfig(
  option: ConfigOptionDescriptor | null | undefined,
): Array<{ id: string; label: string }> | null {
  if (!option) return null;
  const seen = new Map<string, { id: string; label: string }>();
  for (const item of option.options) {
    const base = uiBaseFromModelValue(item.value);
    if (!seen.has(base)) {
      seen.set(base, {
        id: base,
        label: stripCursorMetadataLabel(stripFastLabel(item.name)),
      });
    }
  }
  return [...seen.values()];
}

function configModelSupportsFast(
  option: ConfigOptionDescriptor | null | undefined,
  baseModel: string,
): boolean {
  if (!option) return modelSupportsFast("cursor", baseModel);
  return option.options.some((item) => {
    const parsed = parseCursorModelValue(item.value);
    return uiBaseFromModelValue(item.value) === baseModel && parsed.fast;
  });
}

function configHasAnyFast(option: ConfigOptionDescriptor | null | undefined): boolean {
  if (!option) return agentBaseModelOptions("cursor").some((item) => modelSupportsFast("cursor", item.id));
  return option.options.some((item) => {
    const parsed = parseCursorModelValue(item.value);
    return parsed.fast;
  });
}

function resolveConfigModelValue(
  option: ConfigOptionDescriptor | null | undefined,
  baseModel: string,
  fast: boolean,
): string | null {
  if (baseModel === "auto") {
    if (!option) return "auto";
    return (
      option.options.find((item) => uiBaseFromModelValue(item.value) === "auto")
        ?.value ?? null
    );
  }
  if (!option) return composeAgentModel("cursor", baseModel, fast);

  const exact = option.options.find((item) => {
    const parsed = parseCursorModelValue(item.value);
    const itemBase = uiBaseFromModelValue(item.value);
    if (itemBase !== baseModel) return false;
    if (fast) return parsed.fast;
    return parsed.hasFastParam ? !parsed.fast : true;
  });
  if (exact) return exact.value;

  return null;
}

export function CursorModelControls({
  currentAgent,
  sessionTool,
  currentModel,
  modelConfigOption,
  fastConfigOption,
  onSetConfigOption,
}: Props) {
  const [open, setOpen] = useState(false);
  const [query, setQuery] = useState("");
  const [pendingModel, setPendingModel] = useState<string | null>(null);
  const [pendingFastValue, setPendingFastValue] = useState<string | null>(null);
  const [fastMode, setFastMode] = useState(false);
  const ref = useRef<HTMLDivElement | null>(null);
  const menuId = "cursor-model-menu";
  const configuredOptions = baseOptionsFromConfig(modelConfigOption);
  const options = configuredOptions ?? agentBaseModelOptions("cursor");
  const hasParameterizedFast = fastConfigOption != null;
  const fastAvailable = hasParameterizedFast
    ? fastConfigOption.options.some((option) => option.value === "true")
    : configHasAnyFast(modelConfigOption);
  const confirmedFastValue = fastConfigOption?.current_value ?? "false";
  const fastPending =
    pendingFastValue != null && pendingFastValue !== confirmedFastValue;
  const effectiveFastValue = fastPending
    ? pendingFastValue
    : confirmedFastValue;
  const modelOptionFast = hasParameterizedFast ? false : fastMode;
  const visibleOptions = useMemo(
    () =>
      options
        .filter((option) =>
          resolveConfigModelValue(modelConfigOption, option.id, modelOptionFast) != null,
        )
        .filter((option) =>
          optionMatches(
            option,
            query,
            configModelSupportsFast(modelConfigOption, option.id)
              ? `${option.id}-fast ${option.label} Fast`
              : "",
          ),
        )
        .slice(0, 12),
    [modelConfigOption, modelOptionFast, options, query],
  );

  const closeMenu = useCallback(() => {
    setOpen(false);
    setQuery("");
  }, []);

  useEffect(() => {
    if (!open) return;
    const onClick = (e: MouseEvent) => {
      if (!ref.current?.contains(e.target as Node)) closeMenu();
    };
    const onKey = (e: KeyboardEvent) => {
      if (e.key === "Escape") closeMenu();
    };
    document.addEventListener("mousedown", onClick);
    document.addEventListener("keydown", onKey);
    return () => {
      document.removeEventListener("mousedown", onClick);
      document.removeEventListener("keydown", onKey);
    };
  }, [closeMenu, open]);

  const cursorSession =
    sessionTool === "cursor" ||
    currentAgent === "cursor" ||
    currentAgent === "cursor-agent";
  if (!cursorSession) return null;

  const currentSourceModel = modelConfigOption?.current_value ?? currentModel;
  const effectiveModel = pendingModel ?? normalizedModel(currentSourceModel);
  const baseModel = uiBaseFromModelValue(effectiveModel) || "auto";
  const fastSupported = configModelSupportsFast(modelConfigOption, baseModel);
  const fastChecked = hasParameterizedFast
    ? effectiveFastValue === "true"
    : fastMode;
  const current =
    options.find((option) => option.id === baseModel) ?? {
      id: baseModel,
      label: baseModel,
    };
  const currentNormalizedValue = normalizedModel(currentSourceModel);

  const commitModel = async (nextBaseModel: string, nextFast: boolean) => {
    const nextModel = resolveConfigModelValue(
      modelConfigOption,
      nextBaseModel,
      hasParameterizedFast ? false : nextFast,
    );
    if (nextModel == null) return;
    setPendingModel(nextModel);
    try {
      if (modelConfigOption && onSetConfigOption) {
        await onSetConfigOption(modelConfigOption.id, nextModel);
      }
    } finally {
      setPendingModel((pending) => (pending === nextModel ? null : pending));
    }
  };

  const commitFast = async (nextFast: boolean) => {
    if (fastConfigOption && onSetConfigOption) {
      const nextValue = nextFast ? "true" : "false";
      setPendingFastValue(nextValue);
      try {
        await onSetConfigOption(fastConfigOption.id, nextValue);
      } catch (e) {
        setPendingFastValue((pending) =>
          pending === nextValue ? null : pending,
        );
        throw e;
      }
      return;
    }

    setFastMode(nextFast);
    if (fastSupported) void commitModel(baseModel, nextFast);
  };

  return (
    <div
      ref={ref}
      data-testid="cursor-model-controls"
      className="flex items-center gap-1.5"
    >
      <div className="relative">
        <button
          type="button"
          onClick={() => {
            if (open) {
              closeMenu();
            } else {
              setOpen(true);
            }
          }}
          aria-haspopup="menu"
          aria-expanded={open}
          aria-controls={open ? menuId : undefined}
          aria-label={`Cursor model: ${current.label}`}
          title={`Cursor model: ${current.label}`}
          data-testid="cursor-model-trigger"
          className={[
            "inline-flex items-center gap-1 rounded-md border border-surface-700 bg-surface-800/60 px-2 py-1 text-[11px] font-medium",
            "text-text-secondary transition-colors hover:border-brand-600/60 hover:text-text-primary",
            pendingModel ? "opacity-60" : "",
          ].join(" ")}
        >
          <span className="max-w-32 truncate">{current.label}</span>
          <ChevronUp className="h-3 w-3 opacity-70" />
        </button>
        {open && (
          <div
            id={menuId}
            role="menu"
            className="absolute bottom-full left-0 z-30 mb-1 w-72 overflow-hidden rounded-md border border-surface-700 bg-surface-850 shadow-xl"
          >
            <div className="border-b border-surface-800 px-3 py-1.5 text-[10px] uppercase tracking-wider text-text-dim">
              Cursor model
            </div>
            <div className="border-b border-surface-800 p-2">
              <input
                type="text"
                role="combobox"
                aria-label="Search Cursor model"
                value={query}
                onChange={(e) => setQuery(e.target.value)}
                placeholder="Search models"
                className="w-full rounded border border-surface-700 bg-surface-950 px-2 py-1.5 text-[12px] text-text-primary placeholder:text-text-dim focus:border-brand-600 focus:outline-none"
              />
            </div>
            <div className="max-h-72 overflow-y-auto">
              {visibleOptions.map((option) => {
                const optionValue = resolveConfigModelValue(
                  modelConfigOption,
                  option.id,
                  modelOptionFast,
                );
                const isCurrent = optionValue === currentNormalizedValue;
                return (
                  <button
                    key={option.id}
                    type="button"
                    role="menuitem"
                    data-testid={`cursor-model-option-${option.id}`}
                    disabled={pendingModel === option.id}
                    onClick={() => {
                      closeMenu();
                      if (!isCurrent) void commitModel(option.id, fastChecked);
                    }}
                    className={[
                      "flex w-full items-start gap-2 px-3 py-1.5 text-left text-[12px]",
                      isCurrent
                        ? "bg-surface-800 text-text-primary"
                        : "text-text-secondary hover:bg-surface-800 hover:text-text-primary",
                    ].join(" ")}
                  >
                    <span className="flex-1">
                      <span className="block font-medium">{option.label}</span>
                      <span className="block font-mono text-[11px] text-text-dim">
                        {option.id}
                      </span>
                    </span>
                    {isCurrent && (
                      <span className="text-[10px] uppercase text-brand-500">
                        Active
                      </span>
                    )}
                  </button>
                );
              })}
              {visibleOptions.length === 0 && (
                <div className="px-3 py-2 text-[12px] text-text-dim">
                  No matching models
                </div>
              )}
            </div>
          </div>
        )}
      </div>
      <button
        type="button"
        role="switch"
        aria-label="Cursor Fast mode"
        aria-checked={fastChecked}
        disabled={!fastAvailable || pendingModel !== null || fastPending}
        onClick={() => {
          if (!fastAvailable) return;
          void commitFast(!fastChecked);
        }}
        className={[
          "relative inline-flex h-6 w-10 shrink-0 items-center rounded-full transition-colors",
          "focus-visible:outline-2 focus-visible:outline-offset-2 focus-visible:outline-brand-600",
          fastChecked ? "bg-brand-600" : "bg-surface-700",
          !fastAvailable || pendingModel !== null || fastPending
            ? "cursor-not-allowed opacity-45"
            : "cursor-pointer",
        ].join(" ")}
      >
        <span
          className={[
            "inline-block h-4 w-4 rounded-full bg-white shadow-sm transition-transform",
            fastChecked ? "translate-x-5" : "translate-x-1",
          ].join(" ")}
        />
      </button>
    </div>
  );
}
