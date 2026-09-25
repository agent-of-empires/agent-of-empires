// #1189: resolved theme payloads repaint the dashboard's CSS variables.

import { test, expect } from "./helpers/mockedTest";

interface ResolvedThemePayload {
  name: string;
  source: "builtin" | "custom" | "fallback";
  appearance: "dark" | "light";
  web: { cssVars: Record<string, string> };
  terminal: { cssVars: Record<string, string> };
  syntax: { shikiTheme: string };
}

function dracula(): ResolvedThemePayload {
  return {
    name: "dracula",
    source: "builtin",
    appearance: "dark",
    web: {
      cssVars: {
        "--color-surface-900": "#282a36",
        "--color-surface-950": "#161721",
        "--color-surface-850": "#2f323e",
        "--color-surface-800": "#34374a",
        "--color-surface-700": "#44475a",
        "--color-brand-500": "#ff79c6",
        "--color-brand-400": "#ff9ad4",
        "--color-brand-600": "#d967a8",
        "--color-brand-700": "#b35689",
        "--color-text-primary": "#f8f8f2",
        "--color-text-bright": "#bd93f9",
        "--color-status-running": "#50fa7b",
        "--color-status-waiting": "#ffb86c",
        "--color-status-error": "#ff5555",
        "--color-status-idle": "#6272a4",
      },
    },
    terminal: {
      cssVars: {
        "--term-bg": "#282a36",
        "--term-fg": "#f8f8f2",
        "--term-cursor": "#ff79c6",
        "--term-color-0": "#282a36",
        "--term-color-1": "#ff5555",
        "--term-color-2": "#50fa7b",
        "--term-color-3": "#ffb86c",
      },
    },
    syntax: { shikiTheme: "dracula" },
  };
}

async function stubTheme(
  page: import("@playwright/test").Page,
  byName: Record<string, ResolvedThemePayload>,
  current: ResolvedThemePayload,
) {
  await page.route("**/api/theme/current", (route) =>
    route.fulfill({
      status: 200,
      contentType: "application/json",
      body: JSON.stringify(current),
    }),
  );
  await page.route("**/api/themes/*", (route) => {
    const url = new URL(route.request().url());
    const name = decodeURIComponent(url.pathname.split("/").pop() ?? "");
    const payload = byName[name];
    if (!payload) {
      route.fulfill({ status: 404, body: "not found" });
      return;
    }
    route.fulfill({
      status: 200,
      contentType: "application/json",
      body: JSON.stringify(payload),
    });
  });
}

async function readCssVar(page: import("@playwright/test").Page, name: string): Promise<string> {
  return await page.evaluate((n) => {
    return document.documentElement.style.getPropertyValue(n).trim();
  }, name);
}

test.describe("Theme picker runtime palette swap (#1189)", () => {
  test("pre-React bootstrap paints cached theme before hydration", async ({ page }) => {
    // The cached payload must paint from /theme-bootstrap.js before React; /api/theme/current never resolves, and a CSP
    // violation (an inlined bootstrap) fails the test (#1197).
    const violations: string[] = [];
    page.on("console", (msg) => {
      const t = msg.text();
      if (t.toLowerCase().includes("content security policy")) {
        violations.push(t);
      }
    });
    await page.addInitScript(() => {
      document.addEventListener("securitypolicyviolation", (ev) => {
        console.error(
          "CSP violation:",
          (ev as SecurityPolicyViolationEvent).violatedDirective,
          (ev as SecurityPolicyViolationEvent).blockedURI,
        );
      });
    });
    await page.addInitScript((cached) => {
      localStorage.setItem("aoe-resolved-theme", JSON.stringify(cached));
    }, dracula());
    await page.route("**/api/theme/current", () => {
      // Never fulfilled.
    });
    await page.route("**/api/themes/*", () => {
      // Never fulfilled.
    });

    await page.goto("/");

    await expect
      .poll(() => readCssVar(page, "--color-surface-900"), {
        timeout: 1500,
        intervals: [50, 100, 200],
      })
      .toBe("#282a36");
    const dataTheme = await page.evaluate(() => document.documentElement.dataset.theme);
    expect(dataTheme).toBe("dracula");

    expect(violations).toEqual([]);
  });

  test("the mount-fetched theme repaints the css vars and the chrome elements", async ({ page }) => {
    await stubTheme(page, { dracula: dracula() }, dracula());
    await page.goto("/");
    await expect.poll(() => readCssVar(page, "--color-surface-900")).toBe("#282a36");

    expect(await readCssVar(page, "--color-text-primary")).toBe("#f8f8f2");
    expect(await readCssVar(page, "--term-bg")).toBe("#282a36");

    // Dracula's surface-900 is rgb(40, 42, 54).
    const bg = await page.evaluate(() => getComputedStyle(document.body).backgroundColor);
    expect(bg).toMatch(/40,\s*42,\s*54/);
  });
});
