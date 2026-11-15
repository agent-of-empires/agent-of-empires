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
