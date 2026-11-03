import { useLayoutEffect, useRef, useState } from "react";
import { Activity, Building2, Check, Folder, FolderTree, Layers } from "lucide-react";
import type { SidebarAxis } from "../lib/sidebarAxis";
import { Tooltip } from "./Tooltip";
import { useOutsideDismiss } from "./useOutsideDismiss";
import { TOOLBAR_BUTTON, TOOLBAR_TINT } from "./sidebar/styles";

interface AxisSpec {
  axis: SidebarAxis;
  label: string;
  Icon: typeof Layers;
}

const AXES: readonly AxisSpec[] = [
  { axis: "repo", label: "Repository", Icon: Folder },
  { axis: "org", label: "Organization", Icon: Building2 },
  { axis: "group", label: "User group", Icon: Layers },
  { axis: "repo+group", label: "Repository, then group", Icon: FolderTree },
  { axis: "status", label: "Session status", Icon: Activity },
];

const TOOLTIP: Record<SidebarAxis, string> = {
  repo: "Grouping: by repository",
  org: "Grouping: by organization",
  group: "Grouping: by user group",
  "repo+group": "Grouping: by repository, then user group",
  status: "Grouping: by session status",
};

/** Grouping axis dropdown; every axis is one pick away instead of a trip through a cycle. */
export function SidebarAxisPicker({
  axis,
  onAxisChange,
}: {
  axis: SidebarAxis;
  onAxisChange: (axis: SidebarAxis) => void;
}) {
  const [open, setOpen] = useState(false);
  const ref = useRef<HTMLDivElement>(null);
  const triggerRef = useRef<HTMLButtonElement>(null);
  const optionRefs = useRef<Array<HTMLButtonElement | null>>([]);
  // Set when the menu closes with focus still inside it, so the trigger gets the focus back.
  const restoreFocus = useRef(false);

  useOutsideDismiss(open, [ref], () => setOpen(false));

  const activeIndex = Math.max(
    0,
    AXES.findIndex((a) => a.axis === axis),
  );

  // Menu-button pattern: an option takes focus on open, so arrows and Escape reach the menu
  // instead of only Tab. The checked one, since picking is what a reader came for.
  useLayoutEffect(() => {
    if (open) optionRefs.current[activeIndex]?.focus({ preventScroll: true });
  }, [open, activeIndex]);

  // A picked option unmounts with the menu, taking the focus with it; hand it back to the
  // trigger once the menu is gone, after the axis change has re-rendered.
  useLayoutEffect(() => {
    if (open || !restoreFocus.current) return;
    restoreFocus.current = false;
    triggerRef.current?.focus({ preventScroll: true });
  }, [open]);

  const moveFocus = (from: number) => {
    const count = AXES.length;
    optionRefs.current[((from % count) + count) % count]?.focus({
      preventScroll: true,
    });
  };

  const onMenuKeyDown = (e: React.KeyboardEvent) => {
    const focused = optionRefs.current.findIndex((el) => el === document.activeElement);
    switch (e.key) {
      case "ArrowDown":
        e.preventDefault();
        moveFocus(focused + 1);
        break;
      case "ArrowUp":
        e.preventDefault();
        moveFocus(focused - 1);
        break;
      case "Home":
        e.preventDefault();
        moveFocus(0);
        break;
      case "End":
        e.preventDefault();
        moveFocus(AXES.length - 1);
        break;
      case "Escape":
        e.preventDefault();
        restoreFocus.current = true;
        setOpen(false);
        break;
    }
  };

  return (
    <div
      ref={ref}
      className="relative"
      // Tabbing out closes the menu; moving between its own children is not a dismiss.
      onBlur={(e) => {
        const next = e.relatedTarget as Node | null;
        if (next && !ref.current?.contains(next)) setOpen(false);
      }}
    >
      <Tooltip text={TOOLTIP[axis]} open={open ? false : undefined}>
        <button
          ref={triggerRef}
          onClick={() => setOpen((o) => !o)}
          aria-haspopup="menu"
          aria-expanded={open}
          aria-label={`Group sessions, current: ${AXES[activeIndex]!.label}`}
          data-testid="sidebar-axis-toggle"
          data-axis={axis}
          className={`${TOOLBAR_BUTTON} ${TOOLBAR_TINT(axis !== "repo")}`}
        >
          <Layers className="h-3.5 w-3.5" />
        </button>
      </Tooltip>

      {open && (
        <div
          role="menu"
          data-testid="sidebar-axis-menu"
          onKeyDown={onMenuKeyDown}
          className="absolute right-0 top-full mt-1 min-w-[190px] bg-surface-800 border border-surface-700/50 rounded-md shadow-xl py-1 z-50 animate-fade-in"
        >
          {AXES.map(({ axis: value, label, Icon }, index) => {
            const selected = value === axis;
            return (
              <button
                key={value}
                ref={(el) => {
                  optionRefs.current[index] = el;
                }}
                type="button"
                role="menuitemradio"
                aria-checked={selected}
                // Roving focus: the menu owns the arrows, so options are not Tab stops.
                tabIndex={-1}
                data-testid={`sidebar-axis-option-${value}`}
                onClick={() => {
                  restoreFocus.current = true;
                  setOpen(false);
                  if (!selected) onAxisChange(value);
                }}
                className={`w-full flex items-center gap-2 px-3 py-1.5 text-sm cursor-pointer hover:bg-surface-700/60 ${
                  selected ? "text-brand-500" : "text-text-secondary hover:text-text-primary"
                }`}
              >
                <Icon className="h-3.5 w-3.5 shrink-0" />
                <span className="flex-1 text-left">{label}</span>
                {selected && <Check className="h-3.5 w-3.5 shrink-0" />}
              </button>
            );
          })}
        </div>
      )}
    </div>
  );
}
