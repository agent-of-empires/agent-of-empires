import { Fragment, type ReactNode } from "react";

import type { DockLocation } from "../lib/panes";
import { Dock, type PaneDisplay } from "./Dock";

/** One rendered group: its persisted index, its visible tabs, and the active
 *  one. The parent filters out groups with no visible tab but keeps each
 *  surviving group's persisted index, so drops address the right group. */
export interface DockGroupView {
  group: number;
  tabs: string[];
  active: string | null;
}

interface Props {
  location: DockLocation;
  groups: DockGroupView[];
  descriptorFor: (id: string) => PaneDisplay;
  renderBody: (id: string) => ReactNode;
  onActivate: (id: string) => void;
  onClose: (id: string) => void;
  onMove: (id: string, dock: DockLocation) => void;
  onNewTerminal?: () => void;
}

/** Lays a dock's groups out along its split axis: the tall right column stacks
 *  groups top to bottom (flex column), the wide bottom strip places groups
 *  side by side (flex row). Groups share space equally; a thin divider
 *  separates them. Equal flex sizing is intentional until per-group resize
 *  handles are added (`#2486` follow-up). */
export function DockGroups({
  location,
  groups,
  descriptorFor,
  renderBody,
  onActivate,
  onClose,
  onMove,
  onNewTerminal,
}: Props) {
  if (groups.length === 0) return null;
  const axis = location === "right" ? "flex-col" : "flex-row";
  const divider = location === "right" ? "border-t" : "border-l";
  return (
    <div className={`flex ${axis} min-h-0 min-w-0 flex-1 overflow-hidden`}>
      {groups.map((g, i) => (
        <Fragment key={g.group}>
          <div
            className={`flex min-h-0 min-w-0 flex-1 overflow-hidden ${i > 0 ? `${divider} border-surface-700/40` : ""}`}
          >
            <Dock
              location={location}
              groupIndex={g.group}
              tabs={g.tabs}
              active={g.active}
              descriptorFor={descriptorFor}
              renderBody={renderBody}
              onActivate={onActivate}
              onClose={onClose}
              onMove={onMove}
              onNewTerminal={onNewTerminal}
            />
          </div>
        </Fragment>
      ))}
    </div>
  );
}
