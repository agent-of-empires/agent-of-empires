import { useCallback, useEffect, useMemo, useState } from "react";
import type { RepoGroup } from "../lib/types";
import { safeGetItem, safeRemoveItem, safeSetItem } from "../lib/safeStorage";
import { buildOrgGroups, type OrgNestedGroup } from "../lib/sidebarGroups";

// Distinct from the repo prefix (`aoe-repo-collapsed-`) and the nested
// subgroup prefix (`aoe-nested-group-collapsed-`): a repo's collapse state
// under an org is independent of both, and an org header is a collapsible
// element with no equivalent on any other axis. See #3283.
const COLLAPSED_KEY_PREFIX = "aoe-org-group-collapsed-";

function orgKey(orgId: string): string {
  return `org:${encodeURIComponent(orgId)}`;
}

function repoKey(orgId: string, repoId: string): string {
  return `repo:${encodeURIComponent(orgId)}::${encodeURIComponent(repoId)}`;
}

function loadCollapsed(key: string): boolean {
  return safeGetItem(`${COLLAPSED_KEY_PREFIX}${key}`) === "1";
}

export function useOrgGroups(repoGroups: RepoGroup[]): {
  groups: OrgNestedGroup[];
  toggleOrgCollapsed: (orgId: string) => void;
  toggleRepoCollapsed: (orgId: string, repoId: string) => void;
} {
  const [collapsedMap, setCollapsedMap] = useState<Record<string, boolean>>({});

  const groups = useMemo(
    () =>
      buildOrgGroups(repoGroups, {
        isOrgCollapsed: (orgId) => {
          const key = orgKey(orgId);
          return collapsedMap[key] ?? loadCollapsed(key);
        },
        isRepoCollapsed: (orgId, repoId) => {
          const key = repoKey(orgId, repoId);
          return collapsedMap[key] ?? loadCollapsed(key);
        },
      }),
    [repoGroups, collapsedMap],
  );

  // The updater stays pure and persistence runs in an effect, for the same
  // StrictMode double-invoke reason documented in `useSessionGroups`.
  const toggleOrgCollapsed = useCallback((orgId: string) => {
    const key = orgKey(orgId);
    setCollapsedMap((prev) => {
      const current = prev[key] ?? loadCollapsed(key);
      return { ...prev, [key]: !current };
    });
  }, []);

  const toggleRepoCollapsed = useCallback((orgId: string, repoId: string) => {
    const key = repoKey(orgId, repoId);
    setCollapsedMap((prev) => {
      const current = prev[key] ?? loadCollapsed(key);
      return { ...prev, [key]: !current };
    });
  }, []);

  useEffect(() => {
    for (const [key, collapsed] of Object.entries(collapsedMap)) {
      if (collapsed) {
        safeSetItem(`${COLLAPSED_KEY_PREFIX}${key}`, "1");
      } else {
        safeRemoveItem(`${COLLAPSED_KEY_PREFIX}${key}`);
      }
    }
  }, [collapsedMap]);

  return { groups, toggleOrgCollapsed, toggleRepoCollapsed };
}
