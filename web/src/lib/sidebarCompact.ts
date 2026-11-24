import { createContext, useContext } from "react";

/** Compact sidebar flag for the whole subtree. Lives here to avoid an import cycle; false without a provider. */
export const SidebarCompactContext = createContext(false);

export function useSidebarCompact(): boolean {
  return useContext(SidebarCompactContext);
}
