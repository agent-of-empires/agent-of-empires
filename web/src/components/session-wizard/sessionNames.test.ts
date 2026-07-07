import { describe, expect, it } from "vitest";
import { applyBranchOverride, getReviewSummary, slugifyBranch } from "./sessionNames";

describe("applyBranchOverride", () => {
  it("marks a non-empty branch as a manual override", () => {
    expect(applyBranchOverride("session-title", "feature/custom")).toEqual({
      worktreeBranch: "feature/custom",
      worktreeBranchDirty: true,
    });
  });

  it("honors an explicit clear and marks the field dirty so the title mirror stops", () => {
    expect(applyBranchOverride("session-title", "")).toEqual({
      worktreeBranch: "",
      worktreeBranchDirty: true,
    });
  });

  it("keeps both fields empty when there is no title to fall back to", () => {
    expect(applyBranchOverride("", "")).toEqual({
      worktreeBranch: "",
      worktreeBranchDirty: true,
    });
  });
});

describe("slugifyBranch", () => {
  it("kebab-cases titles with spaces", () => {
    expect(slugifyBranch("Exploration and issues v2")).toBe("exploration-and-issues-v2");
  });

  it("collapses runs of punctuation into a single dash", () => {
    expect(slugifyBranch("Fix: login @ mobile #42")).toBe("fix-login-mobile-42");
  });

  it("replaces forward slashes; git allows them but the slug stays kebab", () => {
    expect(slugifyBranch("feat/auth.refactor")).toBe("feat-auth-refactor");
  });

  it("folds Latin diacritics and ligatures", () => {
    expect(slugifyBranch("café fix")).toBe("cafe-fix");
    expect(slugifyBranch("Straße")).toBe("strasse");
    expect(slugifyBranch("œuvre")).toBe("oeuvre");
  });

  it("strips leading and trailing punctuation", () => {
    expect(slugifyBranch("  hello world!  ")).toBe("hello-world");
  });

  it("falls back to 'session' when nothing survives", () => {
    expect(slugifyBranch("")).toBe("session");
    expect(slugifyBranch("---")).toBe("session");
    expect(slugifyBranch("🚀")).toBe("session");
  });
});

describe("getReviewSummary", () => {
  it("shows the branch when the title is blank because the backend reuses it", () => {
    expect(getReviewSummary("", "feature/custom")).toEqual({
      title: "feature/custom",
      branch: "feature/custom",
    });
  });

  it("shows the title-derived branch when no explicit branch is set", () => {
    expect(getReviewSummary("session-title", "")).toEqual({
      title: "session-title",
      branch: "session-title",
    });
  });
});
