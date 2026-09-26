import { describe, it, expect } from "vitest";
import { extensionToLanguage } from "./language";

describe("extensionToLanguage", () => {
  it.each([
    ["src/main.rs", "rust"],
    ["docker/Dockerfile", "dockerfile"],
    ["DOCKERFILE", "dockerfile"],
    ["foo.xyz", ""],
    ["Makefile", ""],
  ])("maps %s -> %s", (path, lang) => {
    expect(extensionToLanguage(path)).toBe(lang);
  });
});
