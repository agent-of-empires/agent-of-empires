// Tests for the SessionWizard reducer's APPLY_PROFILE_DEFAULTS path,
// added in #1142 so the web wizard now seeds yoloMode/sandboxEnabled/
// tool/extraEnv from the active profile on mount instead of waiting for
// the user to flip the (often-hidden) profile picker.
//
// The reducer is the seam: the mount-time effect dispatches the same
// action the picker does, so unit-testing the reducer covers the
// per-field merge rules without standing up React + the wizard fetch
// graph.

import { describe, expect, it } from "vitest";

import { initialData, reducer, type WizardState } from "./wizardReducer";

function makeState(overrides: Partial<WizardState> = {}): WizardState {
  return {
    data: { ...initialData },
    isSubmitting: false,
    error: null,
    agents: [],
    groups: [],
    profiles: [],
    dockerAvailable: false,
    ...overrides,
  };
}

describe("SessionWizard reducer / APPLY_PROFILE_DEFAULTS (#1142)", () => {
  it("seeds yoloMode from a profile-resolved fetch on mount", () => {
    // Simulates the mount-time path: the user never touched the picker,
    // and /api/settings?profile=<active> resolved with yolo_mode_default
    // = true. Before #1142 the wizard ignored this and stayed at false.
    const next = reducer(makeState(), {
      type: "APPLY_PROFILE_DEFAULTS",
      yoloMode: true,
      sandboxEnabled: false,
      tool: "claude",
      extraEnv: [],
      skipIfDirty: true,
    });
    expect(next.data.yoloMode).toBe(true);
    expect(next.data.sandboxEnabled).toBe(false);
    expect(next.data.tool).toBe("claude");
    expect(next.data.profileDirty).toBe(false);
  });

  it("seeds sandboxEnabled and extraEnv together so the env list survives", () => {
    const next = reducer(makeState(), {
      type: "APPLY_PROFILE_DEFAULTS",
      yoloMode: false,
      sandboxEnabled: true,
      tool: "claude",
      extraEnv: ["FOO=1", "BAR=baz"],
      skipIfDirty: true,
    });
    expect(next.data.sandboxEnabled).toBe(true);
    expect(next.data.extraEnv).toEqual(["FOO=1", "BAR=baz"]);
  });

  it("falls back to the existing tool when the profile reports an empty default_tool", () => {
    // `(session?.default_tool as string) || ""` resolves empty when the
    // profile doesn't set a tool; the reducer must keep whatever the
    // wizard already had (the prefill or "claude" default).
    const next = reducer(makeState({ data: { ...initialData, tool: "opencode" } }), {
      type: "APPLY_PROFILE_DEFAULTS",
      yoloMode: false,
      sandboxEnabled: false,
      tool: "",
      extraEnv: [],
      skipIfDirty: true,
    });
    expect(next.data.tool).toBe("opencode");
  });

  it("respects skipIfDirty: a slow mount fetch must not clobber user edits", () => {
    // The race the reducer guards against: the user toggled yoloMode off
    // (after picking a profile) before /api/settings resolved. The
    // mount-time dispatch sets skipIfDirty so the late response is a
    // no-op instead of stomping back to the profile default.
    const dirty = makeState({
      data: {
        ...initialData,
        profile: "team-defaults",
        profileDirty: true,
        yoloMode: false,
      },
    });
    const next = reducer(dirty, {
      type: "APPLY_PROFILE_DEFAULTS",
      yoloMode: true,
      sandboxEnabled: true,
      tool: "claude",
      extraEnv: ["FOO=1"],
      skipIfDirty: true,
    });
    expect(next).toBe(dirty);
  });

  it("ignores skipIfDirty for the picker-driven path so confirmed overrides apply", () => {
    // `AgentStep.handleProfileChange` shows a window.confirm() before
    // dispatching with skipIfDirty omitted/false. Even with
    // profileDirty: true, the action must apply.
    const dirty = makeState({
      data: {
        ...initialData,
        profile: "team-defaults",
        profileDirty: true,
        yoloMode: false,
      },
    });
    const next = reducer(dirty, {
      type: "APPLY_PROFILE_DEFAULTS",
      yoloMode: true,
      sandboxEnabled: true,
      tool: "claude",
      extraEnv: [],
    });
    expect(next.data.yoloMode).toBe(true);
    expect(next.data.profileDirty).toBe(false);
  });

  it("enabling scratch clears path, extraRepoPaths, and useWorktree", () => {
    // Mutual exclusion: switching to scratch must not leave stale
    // path/useWorktree state that would otherwise leak into the submit
    // payload (the server would 400 on scratch + worktree_branch, and
    // the UI would render an empty path next to a "real" project marker).
    const seeded = makeState({
      data: {
        ...initialData,
        path: "/Users/me/old-project",
        extraRepoPaths: ["/Users/me/lib-a", "/Users/me/lib-b"],
        useWorktree: true,
      },
    });
    const next = reducer(seeded, {
      type: "SET_FIELD",
      field: "scratch",
      value: true,
    });
    expect(next.data.scratch).toBe(true);
    expect(next.data.path).toBe("");
    expect(next.data.extraRepoPaths).toEqual([]);
    expect(next.data.useWorktree).toBe(false);
  });

  it("setting a real path clears scratch (bidirectional reset)", () => {
    const seeded = makeState({
      data: { ...initialData, scratch: true, path: "" },
    });
    const next = reducer(seeded, {
      type: "SET_FIELD",
      field: "path",
      value: "/Users/me/picked-project",
    });
    expect(next.data.scratch).toBe(false);
    expect(next.data.path).toBe("/Users/me/picked-project");
  });

  it("setting extraRepoPaths to a non-empty array clears scratch", () => {
    const seeded = makeState({
      data: { ...initialData, scratch: true },
    });
    const next = reducer(seeded, {
      type: "SET_FIELD",
      field: "extraRepoPaths",
      value: ["/Users/me/lib"],
    });
    expect(next.data.scratch).toBe(false);
  });

  it("setting scratch to false does NOT clear an existing path", () => {
    // A redundant SET_FIELD scratch=false (e.g. user toggles off and
    // then back to a real project) must not wipe whatever path the
    // user just picked.
    const seeded = makeState({
      data: { ...initialData, scratch: false, path: "/Users/me/keep-me" },
    });
    const next = reducer(seeded, {
      type: "SET_FIELD",
      field: "scratch",
      value: false,
    });
    expect(next.data.path).toBe("/Users/me/keep-me");
  });

  it("marks dirty on user toggles even without a profile selected", () => {
    // The dirty guard initially only fired when state.data.profile was
    // truthy, which left no-prefill / no-active-profile users exposed
    // to a race: a fast yoloMode toggle before /api/settings resolved
    // wouldn't set profileDirty, so the late APPLY_PROFILE_DEFAULTS
    // with skipIfDirty: true would still stomp the edit. Now any
    // SET_FIELD on yoloMode/sandboxEnabled/tool/extraEnv marks dirty.
    const fresh = reducer(makeState(), {
      type: "SET_FIELD",
      field: "yoloMode",
      value: true,
    });
    expect(fresh.data.profile).toBe("");
    expect(fresh.data.profileDirty).toBe(true);

    // Verify the dirty flag protects against the late mount fetch.
    const late = reducer(fresh, {
      type: "APPLY_PROFILE_DEFAULTS",
      yoloMode: false,
      sandboxEnabled: false,
      tool: "claude",
      extraEnv: [],
      skipIfDirty: true,
    });
    expect(late).toBe(fresh);
    expect(late.data.yoloMode).toBe(true);
  });
});

describe("SessionWizard reducer / useStructuredView (#1580)", () => {
  it("defaults useStructuredView to true so ACP-capable tools use the structured view by default", () => {
    expect(initialData.useStructuredView).toBe(true);
  });

  it("SET_FIELD useStructuredView updates the flag", () => {
    const next = reducer(makeState(), {
      type: "SET_FIELD",
      field: "useStructuredView",
      value: false,
    });
    expect(next.data.useStructuredView).toBe(false);
  });

  it("toggling useStructuredView does NOT mark profileDirty", () => {
    // useStructuredView is deliberately excluded from the dirty-tracking list:
    // the mount-time APPLY_PROFILE_DEFAULTS seeder uses skipIfDirty, so
    // marking dirty on a structured view toggle would suppress the profile's
    // tool/yolo/sandbox/env defaults even though structured view is unrelated.
    const next = reducer(makeState(), {
      type: "SET_FIELD",
      field: "useStructuredView",
      value: false,
    });
    expect(next.data.profileDirty).toBe(false);

    // A late profile-defaults fetch must still apply (not be skipped).
    const late = reducer(next, {
      type: "APPLY_PROFILE_DEFAULTS",
      yoloMode: true,
      sandboxEnabled: false,
      tool: "claude",
      extraEnv: [],
      skipIfDirty: true,
    });
    expect(late.data.yoloMode).toBe(true);
  });

  it("switching tool preserves the user's useStructuredView choice", () => {
    const optedOut = reducer(makeState(), {
      type: "SET_FIELD",
      field: "useStructuredView",
      value: false,
    });
    const next = reducer(optedOut, {
      type: "SET_FIELD",
      field: "tool",
      value: "opencode",
    });
    expect(next.data.tool).toBe("opencode");
    expect(next.data.useStructuredView).toBe(false);
  });
});

describe("SessionWizard reducer / SUBMIT_CANCEL (#2045)", () => {
  it("re-enables submit without an error when a pre-create confirm is cancelled", () => {
    // SUBMIT_START disables the button; backing out of the glob volume_ignores
    // confirm modal must restore the interactive state and leave no error.
    const submitting = reducer(makeState(), { type: "SUBMIT_START" });
    expect(submitting.isSubmitting).toBe(true);

    const cancelled = reducer(submitting, { type: "SUBMIT_CANCEL" });
    expect(cancelled.isSubmitting).toBe(false);
    expect(cancelled.error).toBeNull();
  });
});
