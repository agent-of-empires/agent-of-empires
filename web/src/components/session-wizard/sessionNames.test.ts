import { describe, expect, it } from "vitest";
import { slugifyBranch } from "./sessionNames";

describe("slugifyBranch", () => {
  it.each([
    ["Exploration and issues v2", "exploration-and-issues-v2"],
    ["Fix: login @ mobile #42", "fix-login-mobile-42"],
    ["feat/auth.refactor", "feat-auth-refactor"],
    ["café fix", "cafe-fix"],
    ["Straße", "strasse"],
    ["œuvre", "oeuvre"],
    ["  hello world!  ", "hello-world"],
    ["", "session"],
    ["---", "session"],
    ["🚀", "session"],
  ])("%j -> %j", (title, slug) => {
    expect(slugifyBranch(title)).toBe(slug);
  });
});
