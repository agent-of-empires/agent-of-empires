import { useEffect, type ComponentProps, type ReactNode } from "react";
import { useSortable } from "@dnd-kit/sortable";
import { CSS } from "@dnd-kit/utilities";
import { useDragSuppressRef } from "./dnd";
import { SessionRow } from "./SessionRow";
import type { DragHandleProps } from "./types";

const liftClass = (isDragging: boolean) =>
  "transition-shadow duration-150 " + (isDragging ? "ring-2 ring-inset ring-brand-500 shadow-lg" : "");

/** A session row that is its own drag handle: a tap navigates, a press-and-hold reorders. */
export function SortableSessionRow({
  rowKey,
  dragDisabled,
  ...props
}: Omit<ComponentProps<typeof SessionRow>, "indented"> & { rowKey?: string; dragDisabled?: boolean }) {
  const dragSuppressRef = useDragSuppressRef();
  const dragOff = !!props.readOnly || !!dragDisabled;
  const { listeners, setNodeRef, transform, transition, isDragging } = useSortable({
    id: rowKey ?? props.workspace.id,
    disabled: dragOff,
    data: { type: "workspace" },
  });
  useEffect(() => {
    if (isDragging) {
      dragSuppressRef.current = Date.now() + 1000;
    } else if (dragSuppressRef.current > Date.now()) {
      // Long enough to swallow the synthetic release click, short enough for a real tap after.
      dragSuppressRef.current = Date.now() + 250;
    }
  }, [isDragging, dragSuppressRef]);
  return (
    // Only `listeners`: dnd-kit's `attributes` would add a second focusable button role.
    <div
      ref={setNodeRef}
      style={{
        transform: CSS.Transform.toString(transform),
        transition,
        touchAction: "manipulation",
        zIndex: isDragging ? 10 : "auto",
        position: "relative",
      }}
      {...(dragOff ? {} : listeners)}
      aria-roledescription={dragOff ? undefined : "Press and hold to reorder"}
      className={liftClass(isDragging)}
    >
      <SessionRow {...props} indented />
    </div>
  );
}

/** Sortable wrapper around a whole group block (header plus rows) so it moves as a unit. */
export function SortableRepoGroup({
  groupId,
  disabled,
  children,
}: {
  groupId: string;
  disabled: boolean;
  children: (handle: DragHandleProps) => ReactNode;
}) {
  const { attributes, listeners, setNodeRef, setActivatorNodeRef, transform, transition, isDragging } = useSortable({
    id: groupId,
    disabled,
    data: { type: "group" },
  });
  return (
    <div
      ref={setNodeRef}
      style={{
        transform: CSS.Transform.toString(transform),
        transition,
        position: "relative",
        zIndex: isDragging ? 20 : "auto",
      }}
      className={liftClass(isDragging)}
    >
      {children({ setActivatorNodeRef, attributes, listeners, isDragging })}
    </div>
  );
}
