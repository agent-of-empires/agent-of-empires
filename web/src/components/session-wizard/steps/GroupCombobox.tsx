import { useCallback, useId, useLayoutEffect, useRef, useState } from "react";

import type { GroupInfo } from "../../../lib/types";
import { ROW_INPUT } from "../WizardRows";
import { groupOptions, optionLabel, type GroupOption } from "./groupOptions";

const LIST_MAX = 288;

const keyOf = (option: GroupOption) => (option.kind === "group" ? `group:${option.group.path}` : "create");

/** The wizard's Group field: free text with a themed list of the profile's groups. */
export function GroupCombobox({
  id,
  value,
  onChange,
  groups,
}: {
  id: string;
  value: string;
  onChange: (value: string) => void;
  groups: GroupInfo[];
}) {
  const listId = useId();
  const inputRef = useRef<HTMLInputElement>(null);
  const [open, setOpen] = useState(false);
  // By option identity, so groups arriving after typing don't move it onto another row.
  const [highlight, setHighlight] = useState<string | null>(null);
  const [placement, setPlacement] = useState<{ up: boolean; maxHeight: number }>({ up: true, maxHeight: LIST_MAX });
  const { options, highlight: defaultHighlight } = groupOptions(groups, value);

  // Opens upward unless there is room below inside the wizard's scroll area, and never
  // taller than the room it opens into, so the sheet can't clip it.
  const place = useCallback(() => {
    const input = inputRef.current;
    if (!input) return;
    const scroller = input.closest<HTMLElement>("[data-wizard-scroll]");
    const field = input.getBoundingClientRect();
    const area = scroller?.getBoundingClientRect() ?? { top: 0, bottom: window.innerHeight };
    const viewport = window.visualViewport;
    // The visual viewport can sit inside the layout viewport (keyboard up), on either side.
    const top = viewport ? Math.max(area.top, viewport.offsetTop) : area.top;
    const bottom = viewport ? Math.min(area.bottom, viewport.offsetTop + viewport.height) : area.bottom;
    const below = bottom - field.bottom - 8;
    const above = field.top - top - 8;
    const up = below < Math.min(LIST_MAX, above);
    setPlacement({ up, maxHeight: Math.max(0, Math.min(LIST_MAX, up ? above : below)) });
  }, []);

  // Before paint, so the list never shows a frame at its default size.
  useLayoutEffect(() => {
    if (!open) return;
    place();
    const scroller = inputRef.current?.closest<HTMLElement>("[data-wizard-scroll]");
    const viewport = window.visualViewport;
    scroller?.addEventListener("scroll", place, { passive: true });
    viewport?.addEventListener("resize", place);
    // The visual viewport's offset can change without a resize.
    viewport?.addEventListener("scroll", place);
    window.addEventListener("resize", place);
    return () => {
      scroller?.removeEventListener("scroll", place);
      viewport?.removeEventListener("resize", place);
      viewport?.removeEventListener("scroll", place);
      window.removeEventListener("resize", place);
    };
  }, [open, place]);

  const show = (next: string) => {
    setOpen(true);
    const { options: nextOptions, highlight: i } = groupOptions(groups, next);
    setHighlight(i >= 0 ? keyOf(nextOptions[i]!) : null);
  };
  const pick = (option: GroupOption) => {
    if (option.kind === "group") onChange(option.group.path);
    setOpen(false);
  };
  const kept = highlight === null ? -1 : options.findIndex((o) => keyOf(o) === highlight);
  const active = !open ? -1 : highlight === null ? -1 : kept >= 0 ? kept : defaultHighlight;
  const optionId = (i: number) => `${listId}-option-${i}`;
  // Also when rows arrive above the highlighted one or the popup resizes around it.
  useLayoutEffect(() => {
    if (open && active >= 0)
      document.getElementById(`${listId}-option-${active}`)?.scrollIntoView?.({ block: "nearest" });
  }, [open, active, listId, options.length, placement.maxHeight]);
  const noGroups = groups.length === 0;

  return (
    <div className="relative">
      <input
        ref={inputRef}
        id={id}
        type="text"
        role="combobox"
        data-owns-escape
        aria-autocomplete="list"
        aria-expanded={open}
        aria-controls={listId}
        aria-activedescendant={active >= 0 ? optionId(active) : undefined}
        autoComplete="off"
        value={value}
        placeholder="Optional"
        onChange={(e) => {
          onChange(e.target.value);
          show(e.target.value);
        }}
        onFocus={() => show(value)}
        onBlur={() => setOpen(false)}
        onKeyDown={(e) => {
          if (e.key === "ArrowDown" || e.key === "ArrowUp") {
            e.preventDefault();
            if (!open) return show(value);
            if (options.length === 0) return;
            const step = e.key === "ArrowDown" ? 1 : -1;
            const next = Math.min(
              Math.max((active < 0 && step < 0 ? options.length : active) + step, 0),
              options.length - 1,
            );
            setHighlight(keyOf(options[next]!));
          } else if (e.key === "Enter" && open) {
            e.preventDefault();
            if (active >= 0) pick(options[active]!);
            else setOpen(false);
          } else if (e.key === "Escape" && open) {
            e.preventDefault();
            e.stopPropagation();
            setOpen(false);
          }
        }}
        className={ROW_INPUT}
      />
      {open && (
        <div
          data-testid="group-combobox-popup"
          style={{ maxHeight: placement.maxHeight }}
          className={`absolute left-0 right-0 z-20 flex flex-col overflow-hidden rounded-lg border border-surface-700/60 bg-surface-900 shadow-lg ${
            placement.up ? "bottom-full mb-1" : "top-full mt-1"
          }`}
        >
          {/* In a cramped popup the help yields first, so at least one row stays usable. */}
          <ul id={listId} role="listbox" aria-label="Groups" className="min-h-11 flex-1 overflow-y-auto">
            {options.map((option, i) => (
              <li
                key={keyOf(option)}
                id={optionId(i)}
                role="option"
                aria-label={optionLabel(option)}
                aria-selected={i === active}
                onMouseEnter={() => setHighlight(keyOf(option))}
                // Picking on mousedown keeps focus, so blur can't close the list first.
                onMouseDown={(e) => {
                  e.preventDefault();
                  pick(option);
                }}
                className={`flex min-h-11 cursor-pointer items-center justify-between gap-2 px-3 py-1.5 text-sm md:min-h-0 ${
                  i === active ? "bg-surface-800 text-text-primary" : "text-text-secondary"
                }`}
              >
                {option.kind === "group" ? (
                  <>
                    <span className="truncate font-mono">{option.group.path}</span>
                    <span className="shrink-0 text-xs text-text-dim">
                      {option.group.session_count} {option.group.session_count === 1 ? "session" : "sessions"}
                      {option.group.archived && " · archived"}
                    </span>
                  </>
                ) : (
                  <span className="truncate">
                    + Create group "<span className="font-mono">{option.path}</span>"
                  </span>
                )}
              </li>
            ))}
          </ul>
          <p className="min-h-0 shrink overflow-y-auto border-t border-surface-700/60 px-3 py-2 text-xs text-text-dim">
            {noGroups && !value.trim() ? (
              "No groups yet. Type a name to create one."
            ) : (
              <>
                A named heading in the sidebar for sessions you want together, across Projects. Use / to nest, e.g.
                client/api. The sidebar shows Groups when its Layers button is set to group by user group.
              </>
            )}
          </p>
        </div>
      )}
    </div>
  );
}
