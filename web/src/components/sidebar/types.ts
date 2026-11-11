import type { useSortable } from "@dnd-kit/sortable";
import type { BulkTriageBuckets } from "../../lib/sidebarBulk";
import type { Workspace } from "../../lib/types";

export type ClickModifiers = { metaKey: boolean; ctrlKey: boolean; shiftKey: boolean };

/** Row click; `rowKey` identifies the rendered row, since one workspace can be split across buckets. */
export type RowActivate = (rowKey: string, e: ClickModifiers, sessionId: string | null) => void;

/** Whether a row's context menu acts on that row or on the whole multi-selection. */
export type RowContextScope = { kind: "single" } | { kind: "bulk"; count: number; buckets: BulkTriageBuckets };

/** Stable bridge that lets memoized rows drive bulk triage without receiving the selection as props. */
export interface RowBulkApi {
  prepareScope: (rowKey: string) => RowContextScope;
  pin: (workspaces: Workspace[], pinned: boolean) => void;
  archive: (workspaces: Workspace[], archived: boolean) => void;
  snooze: (workspaces: Workspace[], minutes: number | null) => void;
}

/** The group header is the drag activator; only `listeners` are spread, never `attributes`. */
export type DragHandleProps = {
  setActivatorNodeRef: (el: HTMLElement | null) => void;
  attributes: ReturnType<typeof useSortable>["attributes"];
  listeners: ReturnType<typeof useSortable>["listeners"];
};
