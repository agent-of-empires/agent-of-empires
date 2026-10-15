import { describe, expect, it } from "vitest";
import { slugifyBranch } from "./sessionNames";

describe("slugifyBranch", () => {
  it.each([
    ["Fix: login @ mobile #42", "fix-login-mobile-42"],
    ["café fix", "cafe-fix"],
    ["Straße", "strasse"],
    ["  hello world!  ", "hello-world"],
    ["", "session"],
    ["🚀", "session"],
  ])("%j -> %j", (title, slug) => {
    expect(slugifyBranch(title)).toBe(slug);
  });
});
