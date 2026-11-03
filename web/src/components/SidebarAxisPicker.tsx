import { useRef, useState } from "react";
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

  useOutsideDismiss(open, [ref], () => setOpen(false));

  const active = AXES.find((a) => a.axis === axis) ?? AXES[0]!;

  return (
    <div ref={ref} className="relative">
      <Tooltip text={TOOLTIP[axis]}>
        <button
          onClick={() => setOpen((o) => !o)}
          aria-haspopup="menu"
          aria-expanded={open}
          aria-label={`Group sessions, current: ${active.label}`}
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
          className="absolute right-0 top-full mt-1 min-w-[190px] bg-surface-800 border border-surface-700/50 rounded-md shadow-xl py-1 z-50 animate-fade-in"
        >
          {AXES.map(({ axis: value, label, Icon }) => {
            const selected = value === axis;
            return (
              <button
                key={value}
                role="menuitemradio"
                aria-checked={selected}
                data-testid={`sidebar-axis-option-${value}`}
                onClick={() => {
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
