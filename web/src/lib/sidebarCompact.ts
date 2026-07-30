import { createContext, useContext } from "react";

/** Compact (slim) sidebar rail (#2288). A whole-subtree presentation flag: the
 *  memoized session rows, group headers, and the projects footer all need it,
 *  so it rides a context rather than being drilled through the dnd wrappers and
 *  the several render sites. Lives here rather than in WorkspaceSidebar because
 *  ProjectsSection consumes it and WorkspaceSidebar imports ProjectsSection,
 *  which would be a cycle. Mirrors SessionRowTagContext.
 *
 *  Defaults to false: the rows are exported and mounted standalone (unit tests
 *  do this), and no provider means no compact sidebar, so full rendering is the
 *  right answer rather than an error. React.memo does not block context updates.
 */
export const SidebarCompactContext = createContext(false);

export function useSidebarCompact(): boolean {
  return useContext(SidebarCompactContext);
}
