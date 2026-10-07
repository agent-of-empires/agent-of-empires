// @vitest-environment jsdom

import { afterEach, expect, it } from "vitest";
import { currentWebBuildId, isWebUpdateAvailable } from "./webBuildId";

function addModuleScript(src: string) {
  const script = document.createElement("script");
  script.type = "module";
  script.src = src;
  document.head.appendChild(script);
  return script;
}

afterEach(() => {
  document.head.querySelectorAll("script").forEach((s) => s.remove());
});

it("currentWebBuildId reads the hashed entry bundle off the page's own script tag", () => {
  const cases: [string[], string | null][] = [
    [["/assets/index-DKenwdW0.js"], "index-DKenwdW0.js"],
    [["/assets/StructuredView-Abc123.js", "https://example.test/assets/index-Zz9_-x.js"], "index-Zz9_-x.js"],
    [["/src/main.tsx"], null],
  ];
  for (const [srcs, expected] of cases) {
    document.head.querySelectorAll("script").forEach((s) => s.remove());
    srcs.forEach(addModuleScript);
    expect(currentWebBuildId(), srcs.join(",")).toBe(expected);
  }
});

it("isWebUpdateAvailable flags only a mismatch between two known ids", () => {
  expect(isWebUpdateAvailable("index-old.js", "index-new.js")).toBe(true);
  expect(isWebUpdateAvailable("index-same.js", "index-same.js")).toBe(false);
  expect(isWebUpdateAvailable(null, "index-new.js")).toBe(false);
  expect(isWebUpdateAvailable("index-old.js", null)).toBe(false);
  expect(isWebUpdateAvailable("index-old.js", undefined)).toBe(false);
});
