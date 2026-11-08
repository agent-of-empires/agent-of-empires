import { describe, expect, it } from "vitest";

import { makeSession, makeWorkspace } from "../../components/__tests__/fixtures";
import { buildSessionGroups } from "../sidebarGroups";
import { sessionWorkspaces } from "../sessionWorkspace";

const slicesOf = (groups: ReturnType<typeof buildSessionGroups>) =>
  groups.flatMap((g) => g.workspaces.map((v) => v.workspace));

describe("sessionWorkspaces", () => {
  const shared = makeWorkspace("w", [
    makeSession({ id: "s1", group_path: "alpha" }),
    makeSession({ id: "s2", group_path: "beta" }),
    makeSession({ id: "s3", group_path: "beta" }),
  ]);
  const groups = buildSessionGroups([shared], {
    idleDecayWindowMs: 60_000,
    sortMode: "lastActivity",
    isCollapsed: () => false,
  });

  it("builds children for every slice, not just the first one the group axis lists", () => {
    // The group axis splits `w` into [s1] and [s2, s3], both under the id `w`.
    const slices = slicesOf(groups);
    expect(slices.map((s) => s.id)).toEqual(["w", "w"]);

    const children = sessionWorkspaces(slices);
    expect(children.map((c) => c.id).sort()).toEqual(["w::session::s1", "w::session::s2", "w::session::s3"]);
    expect(children.every((c) => c.sessions.length === 1)).toBe(true);
  });

  it("lists a child once however many structures carry its slice", () => {
    const slices = slicesOf(groups);
    expect(sessionWorkspaces([...slices, ...slices])).toHaveLength(3);
  });

  it("keeps a record for a lone session, so an override from when it had a sibling still reconciles", () => {
    const children = sessionWorkspaces([makeWorkspace("solo", [makeSession({ id: "only" })])]);
    expect(children.map((c) => c.id)).toEqual(["solo::session::only"]);
  });
});
