import { useCallback, useMemo } from "react";
import type { RepoGroup } from "../lib/types";
import { buildOrgGroups, type OrgNestedGroup } from "../lib/sidebarGroups";
import { useCollapsedKeys } from "./useCollapsedKeys";

const orgKey = (orgId: string) => `org:${encodeURIComponent(orgId)}`;
const repoKey = (orgId: string, repoId: string) => `repo:${encodeURIComponent(orgId)}::${encodeURIComponent(repoId)}`;

export function useOrgGroups(repoGroups: RepoGroup[]): {
  groups: OrgNestedGroup[];
  toggleOrgCollapsed: (orgId: string) => void;
  toggleRepoCollapsed: (orgId: string, repoId: string) => void;
} {
  const { isCollapsed, toggle } = useCollapsedKeys("aoe-org-group-collapsed-");

  const groups = useMemo(
    () =>
      buildOrgGroups(repoGroups, {
        isOrgCollapsed: (orgId) => isCollapsed(orgKey(orgId)),
        isRepoCollapsed: (orgId, repoId) => isCollapsed(repoKey(orgId, repoId)),
      }),
    [repoGroups, isCollapsed],
  );

  const toggleOrgCollapsed = useCallback((orgId: string) => toggle(orgKey(orgId)), [toggle]);
  const toggleRepoCollapsed = useCallback((orgId: string, repoId: string) => toggle(repoKey(orgId, repoId)), [toggle]);

  return { groups, toggleOrgCollapsed, toggleRepoCollapsed };
}
