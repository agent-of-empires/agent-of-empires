import { describe, expect, it } from "vitest";
import { buildCreateRequest } from "./createRequest";
import { initialData, type WizardData } from "./wizardReducer";

describe("buildCreateRequest workspace", () => {
  it("sends workspace only for a new-branch worktree", () => {
    const cases: [Partial<WizardData>, boolean | undefined][] = [
      [{ useWorktree: true, useWorkspace: true }, true],
      [{ useWorktree: true, useWorkspace: false }, undefined],
      [{ useWorktree: true, useWorkspace: true, attachExisting: true }, undefined],
      [{ useWorktree: false, useWorkspace: true }, undefined],
      [{ scratch: true, useWorktree: true, useWorkspace: true }, undefined],
    ];
    for (const [over, want] of cases) {
      expect(buildCreateRequest({ ...initialData, ...over }, false).workspace, JSON.stringify(over)).toBe(want);
    }
  });
});

describe("buildCreateRequest pull_latest_image", () => {
  it("asks for the pull only for a Structured view Sandbox with the box checked", () => {
    const sandbox = { sandboxEnabled: true, useStructuredView: true };
    const cases: [Partial<WizardData>, boolean, boolean, true | undefined][] = [
      [sandbox, true, true, true],
      [sandbox, true, false, undefined],
      [{ ...sandbox, sandboxEnabled: false }, true, true, undefined],
      [{ ...sandbox, useStructuredView: false }, true, true, undefined],
      [sandbox, false, true, undefined],
    ];
    for (const [over, acpCapable, pull, want] of cases) {
      const body = buildCreateRequest({ ...initialData, ...over }, acpCapable, { pullLatest: pull });
      expect(body.pull_latest_image, JSON.stringify({ over, acpCapable, pull })).toBe(want);
    }
  });
});
