// Playwright `test` wrapper for the mocked suite under `web/tests/`.
//
// The page fixture records browser-side request publication and response-body
// completion for causal negative assertions. It also starts V8 coverage and writes
// it after, so the merged-LCOV pipeline picks up coverage from the mocked
// specs the same way it does from live specs.
//
// Specs do:
//
//   import { test, expect } from "./helpers/mockedTest";
//
// `vite preview` serves the production bundle (built with inline sourcemaps
// when `AOE_COVERAGE=1`). Without that env var, coverage collection is a
// no-op and the override is invisible.

import { test as base, expect, type Page } from "@playwright/test";
import { startCoverage, stopAndWriteCoverage } from "./coverageCapture";

export const test = base.extend({
  page: async ({ page }, use, testInfo) => {
    await page.addInitScript(() => {
      const state = window as typeof window & {
        __mockedRequests: Array<{ path: string; method: string }>;
        __mockedBodies: string[];
      };
      state.__mockedRequests = [];
      state.__mockedBodies = [];
      const originalFetch = window.fetch;
      window.fetch = async (...args) => {
        const [input, init] = args;
        const url = input instanceof Request ? input.url : String(input);
        const path = new URL(url, location.href).pathname;
        state.__mockedRequests.push({
          path,
          method: init?.method ?? (input instanceof Request ? input.method : "GET"),
        });
        const response = await originalFetch(...args);
        const json = response.json.bind(response);
        response.json = async () => {
          const body = await json();
          // The following task runs after the caller's response microtasks.
          setTimeout(() => state.__mockedBodies.push(path), 0);
          return body;
        };
        return response;
      };
    });
    const started = await startCoverage(page);
    await use(page);
    await stopAndWriteCoverage(page, testInfo.titlePath.join(" > "), started);
  },
});

export { expect };

export async function waitForResponseBody(page: Page, path: string) {
  await expect
    .poll(() =>
      page.evaluate(
        (path) => (window as typeof window & { __mockedBodies: string[] }).__mockedBodies.includes(path),
        path,
      ),
    )
    .toBe(true);
}

export function publishedRequests(page: Page, path: string, method: string) {
  return page.evaluate(
    ({ path, method }) =>
      (window as typeof window & { __mockedRequests: Array<{ path: string; method: string }> }).__mockedRequests.filter(
        (request) => request.path === path && request.method === method,
      ),
    { path, method },
  );
}

/** Sample a negative contract for its original browser observation interval. */
export async function observeFor(page: Page, milliseconds: number, assertion: () => Promise<void>) {
  const end = Date.now() + milliseconds;
  do {
    await assertion();
    await page.waitForTimeout(Math.min(16, Math.max(0, end - Date.now())));
  } while (Date.now() < end);
  await assertion();
}
