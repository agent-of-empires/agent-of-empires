import { expect, it } from "vitest";
import { slugifyBranch } from "./sessionNames";

it("slugifyBranch folds punctuation, diacritics, and ligatures, and falls back to session", () => {
  const cases = [
    ["Fix: login @ mobile #42", "fix-login-mobile-42"],
    ["café fix", "cafe-fix"],
    ["Straße", "strasse"],
    ["  hello world!  ", "hello-world"],
    ["", "session"],
    ["🚀", "session"],
  ];
  expect(cases.map(([title]) => [title, slugifyBranch(title!)])).toEqual(cases);
});
