import { Fragment, type ReactNode } from "react";

import type { DockLocation } from "../lib/panes";
import { Dock, type DockControls, type PaneDisplay } from "./Dock";

/** One rendered group: its persisted index, its visible tabs, and the active one. */
export interface DockGroupView {
  group: number;
  tabs: string[];
  active: string | null;
}

interface Props extends DockControls {
  location: DockLocation;
  groups: DockGroupView[];
  descriptorFor: (id: string) => PaneDisplay;
  renderBody: (id: string) => ReactNode;
  onActivate: (id: string) => void;
  onClose: (id: string) => void;
  onMove: (id: string, dock: DockLocation) => void;
}

/** Lays a dock's groups out along its split axis: the tall right column stacks groups top to bottom (flex column),
 *  the wide bottom strip places groups side by side (flex row). */
export function DockGroups({
  location,
  groups,
  descriptorFor,
  renderBody,
  onActivate,
  onClose,
  onMove,
  ...controls
}: Props) {
  if (groups.length === 0) return null;
  const axis = location === "right" ? "flex-col" : "flex-row";
  return (
    <div className={`flex ${axis} gap-2 min-h-0 min-w-0 flex-1 overflow-hidden`}>
      {groups.map((g) => (
        <Fragment key={g.group}>
          {/* Each group is a card on the canvas (see DESIGN.md). */}
          <div className="flex min-h-0 min-w-0 flex-1 overflow-hidden rounded-xl border border-surface-700/45 bg-surface-900 shadow-md">
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
              {...controls}
            />
          </div>
        </Fragment>
      ))}
    </div>
  );
}
