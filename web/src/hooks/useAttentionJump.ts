import { useMemo } from "react";
import type { SidebarGroup } from "../lib/sidebarGroups";
import { usePluginAttention } from "../lib/pluginUiContext";
import { sessionNeedsAttention } from "../lib/sidebarSort";

/** Display-ordered session ids plus the subset needing attention (plugin tones included), for jump-to-next. */
export function useAttentionJump(groups: readonly SidebarGroup[]) {
  const pluginAttention = usePluginAttention();
  return useMemo(() => {
    const orderedIds: string[] = [];
    const attention = new Set<string>();
    for (const g of groups) {
      for (const v of g.workspaces) {
        for (const s of v.workspace.sessions) {
          orderedIds.push(s.id);
          if (sessionNeedsAttention(s, pluginAttention)) attention.add(s.id);
        }
      }
    }
    return { orderedIds, attention };
  }, [groups, pluginAttention]);
}
