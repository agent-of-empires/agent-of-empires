// @vitest-environment jsdom

import { afterEach, describe, expect, it } from "vitest";
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

describe("currentWebBuildId", () => {
  it("reads the hashed entry bundle off the page's own script tag", () => {
    addModuleScript("/assets/index-DKenwdW0.js");
    expect(currentWebBuildId()).toBe("index-DKenwdW0.js");
  });

  it("ignores non-entry module scripts and absolute origins", () => {
    addModuleScript("/assets/StructuredView-Abc123.js");
    addModuleScript("https://example.test/assets/index-Zz9_-x.js");
    expect(currentWebBuildId()).toBe("index-Zz9_-x.js");
  });

  it("returns null on the Vite dev server (unhashed entry)", () => {
    addModuleScript("/src/main.tsx");
    expect(currentWebBuildId()).toBeNull();
  });
});

describe("isWebUpdateAvailable", () => {
  it("flags a mismatch between page and server bundle", () => {
    expect(isWebUpdateAvailable("index-old.js", "index-new.js")).toBe(true);
  });

  it("stays quiet when ids match", () => {
    expect(isWebUpdateAvailable("index-same.js", "index-same.js")).toBe(false);
  });

  it("disables the check when either side is missing", () => {
    expect(isWebUpdateAvailable(null, "index-new.js")).toBe(false);
    expect(isWebUpdateAvailable("index-old.js", null)).toBe(false);
    expect(isWebUpdateAvailable("index-old.js", undefined)).toBe(false);
  });
});
