// @vitest-environment jsdom

import { beforeEach, describe, expect, it } from "vitest";
import { renderHook, act } from "@testing-library/react";

import { useNestedSidebarGroups } from "../useNestedSidebarGroups";
import { useOrgGroups } from "../useOrgGroups";
import { useSessionGroups } from "../useSessionGroups";
import { repoGroup, session, workspace } from "./fixtures";

const groupedWorkspace = (group_path: string, over = {}) =>
  workspace("w1", "/repo", [session({ group_path, branch: "feat", main_repo_path: "/repo", ...over })], {
    branch: "feat",
    displayName: "feat",
  });

const OWNER = "acme@example.com";
const orgRepo = (id = "repo-1") =>
  repoGroup({
    id,
    repoPath: `/${id}`,
    remoteOwner: "acme",
    remoteOwnerKey: OWNER,
    workspaces: [groupedWorkspace("", { remote_owner: "acme", remote_owner_key: OWNER })],
  });

beforeEach(() => localStorage.clear());

describe("sidebar group collapse persistence", () => {
  it("useSessionGroups builds a group per group_path and persists its collapse", () => {
    const render = () => renderHook(() => useSessionGroups([groupedWorkspace("team-a")], "lastActivity"));
    const { result } = render();
    expect(result.current.groups).toHaveLength(1);
    const id = result.current.groups[0]!.id;
    const key = `aoe-group-collapsed-${id}`;

    act(() => result.current.toggleGroupCollapsed(id));
    expect(result.current.groups[0]!.collapsed).toBe(true);
    expect(localStorage.getItem(key)).toBe("1");
    expect(render().result.current.groups[0]!.collapsed).toBe(true);

    act(() => result.current.toggleGroupCollapsed(id));
    expect(result.current.groups[0]!.collapsed).toBe(false);
    expect(localStorage.getItem(key)).toBeNull();
  });

  it("useNestedSidebarGroups keys subgroup collapse on the encoded repo and group path", () => {
    const repos = [repoGroup({ workspaces: [groupedWorkspace("team-a")] })];
    const key = `aoe-nested-group-collapsed-${encodeURIComponent("repo-1")}::${encodeURIComponent("team-a")}`;
    localStorage.setItem(key, "1");
    const { result } = renderHook(() => useNestedSidebarGroups(repos, "lastActivity"));
    const group = () => result.current.groups[0]!;
    expect(group().repo.id).toBe("repo-1");
    expect(group().repo.capabilities.reorder).toBe(false);
    expect(group().subgroups[0]!.collapsed).toBe(true);

    act(() => result.current.toggleSubgroupCollapsed("repo-1", "team-a"));
    expect(localStorage.getItem(key)).toBeNull();
    expect(group().subgroups[0]!.collapsed).toBe(false);
    act(() => result.current.toggleSubgroupCollapsed("repo-1", "team-a"));
    expect(localStorage.getItem(key)).toBe("1");
  });

  it("useOrgGroups buckets repos under their org and collapses org and repo independently", () => {
    const orgKey = `aoe-org-group-collapsed-org:${encodeURIComponent(OWNER)}`;
    const repoKey = `aoe-org-group-collapsed-repo:${encodeURIComponent(OWNER)}::${encodeURIComponent("repo-1")}`;
    const { result } = renderHook(() => useOrgGroups([orgRepo(), orgRepo("repo-2")]));
    const org = () => result.current.groups[0]!;
    expect(org().org.id).toBe(OWNER);
    expect(org().repos.map((r) => r.id)).toEqual(["repo-1", "repo-2"]);

    act(() => result.current.toggleOrgCollapsed(OWNER));
    expect(localStorage.getItem(orgKey)).toBe("1");
    expect(org().org.collapsed).toBe(true);
    expect(org().repos[0]!.collapsed).toBe(false);
    act(() => result.current.toggleOrgCollapsed(OWNER));
    expect(localStorage.getItem(orgKey)).toBeNull();

    act(() => result.current.toggleRepoCollapsed(OWNER, "repo-1"));
    expect(localStorage.getItem(repoKey)).toBe("1");
    expect(org().repos.map((r) => r.collapsed)).toEqual([true, false]);
    expect(org().org.collapsed).toBe(false);
    act(() => result.current.toggleRepoCollapsed(OWNER, "repo-1"));
    expect(localStorage.getItem(repoKey)).toBeNull();
  });

  it("useOrgGroups reads initial org collapse from storage", () => {
    localStorage.setItem(`aoe-org-group-collapsed-org:${encodeURIComponent(OWNER)}`, "1");
    const { result } = renderHook(() => useOrgGroups([orgRepo()]));
    expect(result.current.groups[0]!.org.collapsed).toBe(true);
  });
});
