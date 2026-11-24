// @vitest-environment jsdom

import { beforeEach, describe, expect, it } from "vitest";
import { renderHook, act } from "@testing-library/react";

import { useNestedSidebarGroups } from "../useNestedSidebarGroups";
import { useSessionGroups } from "../useSessionGroups";
import { compareWorkspacesByName } from "../../lib/sidebarSort";
import { repoGroup, session, workspace } from "./fixtures";

const groupedWorkspace = (group_path: string, over = {}) =>
  workspace("w1", "/repo", [session({ group_path, branch: "feat", main_repo_path: "/repo", ...over })], {
    branch: "feat",
    displayName: "feat",
  });

beforeEach(() => localStorage.clear());

describe("sidebar group collapse persistence", () => {
  it("useSessionGroups builds a group per group_path and persists its collapse", () => {
    const render = () => renderHook(() => useSessionGroups([groupedWorkspace("team-a")], compareWorkspacesByName));
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
    const { result } = renderHook(() => useNestedSidebarGroups(repos, compareWorkspacesByName));
    const group = () => result.current.groups[0]!;
    expect(group().repo.id).toBe("repo-1");
    expect(group().subgroups[0]!.collapsed).toBe(true);

    act(() => result.current.toggleSubgroupCollapsed("repo-1", "team-a"));
    expect(localStorage.getItem(key)).toBeNull();
    expect(group().subgroups[0]!.collapsed).toBe(false);
    act(() => result.current.toggleSubgroupCollapsed("repo-1", "team-a"));
    expect(localStorage.getItem(key)).toBe("1");
  });
});
