import { createContext, useContext, useSyncExternalStore } from "react";

/** Compact sidebar flag for the whole subtree. Lives here to avoid an import cycle; false without a provider. */
export const SidebarCompactContext = createContext(false);

export function useSidebarCompact(): boolean {
  return useContext(SidebarCompactContext);
}

const IN_FLOW_QUERY = "(min-width: 768px)";

function subscribeInFlow(onChange: () => void) {
  const query = window.matchMedia?.(IN_FLOW_QUERY);
  query?.addEventListener?.("change", onChange);
  return () => query?.removeEventListener?.("change", onChange);
}

const inFlowSnapshot = () => !!window.matchMedia?.(IN_FLOW_QUERY).matches;

/**
 * Whether the sidebar sits in the layout rather than over it as the phone drawer. Only the in-flow sidebar has
 * the resize handle that enters and leaves the compact rail, so the drawer ignores the saved flag.
 */
export function useSidebarInFlow(): boolean {
  return useSyncExternalStore(subscribeInFlow, inFlowSnapshot);
}
