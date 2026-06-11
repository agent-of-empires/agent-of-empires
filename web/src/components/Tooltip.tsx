import type { ReactNode } from "react";

// Shared hover tooltip used by the sidebar control row (grouping axis, filter,
// new session) and the sort picker. Renders a custom styled span instead of
// the native browser `title` so every control shares one look. Lives in its
// own module so SidebarSortPicker can import it without a cycle back through
// WorkspaceSidebar (which imports SidebarSortPicker).
export function Tooltip({ text, children }: { text: string; children: ReactNode }) {
  return (
    <span className="relative group/tip inline-flex">
      {children}
      <span className="pointer-events-none absolute left-1/2 -translate-x-1/2 top-full mt-1.5 px-2 py-1 rounded bg-surface-950 border border-surface-700 text-[11px] text-text-secondary whitespace-nowrap opacity-0 scale-95 transition-all duration-100 group-hover/tip:opacity-100 group-hover/tip:scale-100 z-50">
        {text}
      </span>
    </span>
  );
}
