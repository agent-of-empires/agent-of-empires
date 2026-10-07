import { describe, expect, it } from "vitest";

import { initialData, reducer, type Action, type WizardData, type WizardState } from "./wizardReducer";

function makeState(data: Partial<WizardData> = {}): WizardState {
  return {
    data: { ...initialData, ...data },
    isSubmitting: false,
    error: null,
    agents: [],
    groups: [],
    profiles: [],
    dockerAvailable: false,
  };
}

const set = (field: string, value: unknown): Action => ({ type: "SET_FIELD", field, value });
const defaults = (over: Partial<Extract<Action, { type: "APPLY_PROFILE_DEFAULTS" }>> = {}): Action => ({
  type: "APPLY_PROFILE_DEFAULTS",
  yoloMode: false,
  sandboxEnabled: false,
  worktreeEnabled: false,
  tool: "claude",
  extraEnv: [],
  skipIfDirty: true,
  ...over,
});
const run = (state: WizardState, ...actions: Action[]) => actions.reduce(reducer, state);

describe("APPLY_PROFILE_DEFAULTS", () => {
  it("seeds profile defaults and clears profileDirty", () => {
    const next = reducer(
      makeState(),
      defaults({ yoloMode: true, sandboxEnabled: true, worktreeEnabled: true, extraEnv: ["FOO=1", "BAR=baz"] }),
    );
    expect(next.data).toMatchObject({
      yoloMode: true,
      sandboxEnabled: true,
      useWorktree: true,
      tool: "claude",
      extraEnv: ["FOO=1", "BAR=baz"],
      profileDirty: false,
    });
  });

  it("keeps the existing tool when the profile reports none", () => {
    expect(reducer(makeState({ tool: "opencode" }), defaults({ tool: "" })).data.tool).toBe("opencode");
  });

  it("never enables worktree for a scratch session", () => {
    expect(reducer(makeState({ scratch: true }), defaults({ worktreeEnabled: true })).data.useWorktree).toBe(false);
  });

  // A remembered or prefilled path resolves its repo probe at mount, often
  // before the chained profile+settings fetch seeds the defaults.
  it.each([
    [false, false],
    [true, true],
  ])("re-enables worktree only where the repo probe said yes (pathIsGitRepo %s)", (pathIsGitRepo, expected) => {
    const state = makeState({ path: "/tmp/p", pathIsGitRepo });
    expect(reducer(state, defaults({ worktreeEnabled: true })).data.useWorktree).toBe(expected);
  });

  it("a late skipIfDirty apply keeps a dirty yoloMode edit but still records the worktree default", () => {
    const edited = reducer(makeState(), set("yoloMode", true));
    expect(edited.data.profileDirty).toBe(true);
    const late = reducer(edited, defaults({ yoloMode: false, worktreeEnabled: true }));
    expect(late.data).toMatchObject({ yoloMode: true, profileWorktreeDefault: true, useWorktree: true });
  });

  it("a late skipIfDirty apply keeps a manual worktree toggle", () => {
    const edited = reducer(makeState(), set("useWorktree", true));
    expect(edited.data).toMatchObject({ profileDirty: true, worktreeDirty: true });
    const late = reducer(edited, defaults({ worktreeEnabled: false }));
    expect(late.data).toMatchObject({ useWorktree: true, profileWorktreeDefault: false });
  });

  it("applies over dirty edits for the confirmed picker path", () => {
    const next = reducer(
      makeState({ profile: "team", profileDirty: true }),
      defaults({ yoloMode: true, skipIfDirty: undefined }),
    );
    expect(next.data.yoloMode).toBe(true);
    expect(next.data.profileDirty).toBe(false);
  });

  it("is not suppressed by a structured view toggle, which does not mark dirty", () => {
    const toggled = reducer(makeState(), set("useStructuredView", false));
    expect(toggled.data.useStructuredView).toBe(false);
    expect(toggled.data.profileDirty).toBe(false);
    expect(reducer(toggled, defaults({ yoloMode: true })).data.yoloMode).toBe(true);
    expect(reducer(toggled, set("tool", "opencode")).data.useStructuredView).toBe(false);
  });

  // #3517: the configured view seeds past other dirty fields; a hand-set view survives mount-time
  // seeding but not a confirmed profile change; an import stays structured.
  it.each([
    ["clean", {}, [], defaults({ useStructuredView: false }), false],
    ["dirty yoloMode", {}, [set("yoloMode", true)], defaults({ useStructuredView: false }), false],
    ["hand-set view", {}, [set("useStructuredView", true)], defaults({ useStructuredView: false }), true],
    [
      "confirmed profile change",
      {},
      [set("useStructuredView", true)],
      defaults({ useStructuredView: false, resetStructuredViewDirty: true, skipIfDirty: undefined }),
      false,
    ],
    ["import", { importAcpSessionId: "abc" }, [], defaults({ useStructuredView: false }), true],
  ] as const)("seeds the configured view: %s", (_, data, edits, seed, view) => {
    expect(run(makeState(data), ...edits, seed).data.useStructuredView).toBe(view);
  });
});

describe("SET_FIELD mutual exclusion", () => {
  it("enabling scratch clears path sources, worktree, import id and a stale non-repo probe", () => {
    const seeded = makeState({
      path: "/old",
      extraRepoPaths: ["/a", "/b"],
      useWorktree: true,
      importAcpSessionId: "abc",
      pathIsGitRepo: false,
    });
    expect(reducer(seeded, set("scratch", true)).data).toMatchObject({
      scratch: true,
      path: "",
      extraRepoPaths: [],
      useWorktree: false,
      importAcpSessionId: "",
      pathIsGitRepo: true,
    });
  });

  it.each([
    ["path", "/picked"],
    ["extraRepoPaths", ["/lib"]],
  ])("a non-empty %s clears scratch and the import id", (field, value) => {
    const next = reducer(makeState({ scratch: true, importAcpSessionId: "abc" }), set(field, value));
    expect(next.data.scratch).toBe(false);
    expect(next.data.importAcpSessionId).toBe("");
  });

  it("disabling scratch keeps the existing path", () => {
    expect(reducer(makeState({ path: "/keep" }), set("scratch", false)).data.path).toBe("/keep");
  });

  it("keeps the import id when the import picker dispatches path then id", () => {
    const next = run(makeState(), set("path", "/cwd"), set("importAcpSessionId", "imp"));
    expect(next.data).toMatchObject({ path: "/cwd", importAcpSessionId: "imp" });
  });

  it("a non-repo probe forces worktree off, and a new path resets the probe optimistically", () => {
    const nonRepo = run(makeState(), set("useWorktree", true), set("pathIsGitRepo", false));
    expect(nonRepo.data.useWorktree).toBe(false);
    expect(reducer(nonRepo, set("path", "/repo")).data.pathIsGitRepo).toBe(true);
  });
});

it("mirrors the title into the branch until the branch is edited, even to empty", () => {
  const mirrored = reducer(makeState(), set("title", "Fix login"));
  expect(mirrored.data.worktreeBranch).toBe("fix-login");
  const cleared = run(mirrored, set("worktreeBranch", ""), set("title", "Other"));
  expect(cleared.data).toMatchObject({ worktreeBranch: "", worktreeBranchDirty: true });
});

it("SUBMIT_CANCEL re-enables submit without an error", () => {
  const cancelled = run(makeState(), { type: "SUBMIT_START" }, { type: "SUBMIT_CANCEL" });
  expect(cancelled.isSubmitting).toBe(false);
  expect(cancelled.error).toBeNull();
});

describe("SEED_PROJECT_WORKTREE_OVERRIDE", () => {
  const seed = (override: boolean | undefined): Action => ({ type: "SEED_PROJECT_WORKTREE_OVERRIDE", override });

  it("applies an override and reverts to the profile default without one", () => {
    const overridden = run(makeState(), defaults({ worktreeEnabled: false }), seed(true));
    expect(overridden.data.useWorktree).toBe(true);
    expect(reducer(overridden, seed(undefined)).data.useWorktree).toBe(false);
  });

  it("outranks a late profile-defaults response", () => {
    const late = run(makeState(), seed(true), defaults({ worktreeEnabled: false }));
    expect(late.data).toMatchObject({ useWorktree: true, profileWorktreeDefault: false });
  });

  it("never enables worktree for a scratch session or a non-repo path", () => {
    expect(reducer(makeState({ scratch: true }), seed(true)).data.useWorktree).toBe(false);
    expect(reducer(makeState({ pathIsGitRepo: false }), seed(true)).data.useWorktree).toBe(false);
  });

  it("does not clobber a manual worktree toggle, but does apply after an unrelated edit", () => {
    expect(run(makeState(), set("useWorktree", true), seed(undefined)).data.useWorktree).toBe(true);
    const toolChanged = reducer(makeState(), set("tool", "codex"));
    expect(toolChanged.data.worktreeDirty).toBe(false);
    expect(reducer(toolChanged, seed(true)).data.useWorktree).toBe(true);
  });

  it("drops a path-scoped seed once the selected path has changed", () => {
    const state = makeState({ path: "/repo/b" });
    const seedFor = (path: string): Action => ({ type: "SEED_PROJECT_WORKTREE_OVERRIDE", override: true, path });
    expect(reducer(state, seedFor("/repo/a"))).toBe(state);
    expect(reducer(state, seedFor("/repo/b")).data.useWorktree).toBe(true);
  });

  it("tracks the latest project while dirty, so a profile switch resolves from it", () => {
    const withB = run(makeState(), seed(true), set("useWorktree", false), seed(false));
    expect(withB.data).toMatchObject({ useWorktree: false, projectWorktreeOverride: false });
    const switched = reducer(withB, defaults({ worktreeEnabled: true, skipIfDirty: false }));
    expect(switched.data).toMatchObject({ useWorktree: false, worktreeDirty: false });
  });
});
