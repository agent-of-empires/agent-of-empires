// Playwright `test` wrapper for the mocked suite under `web/tests/`.
//
// Re-exports `@playwright/test`'s `test` and `expect` with one override:
// the `page` fixture starts Chromium V8 coverage before the test and writes
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

import { test as base, expect } from "@playwright/test";
import { startCoverage, stopAndWriteCoverage } from "./coverageCapture";

export const test = base.extend({
  page: async ({ page }, use, testInfo) => {
    const started = await startCoverage(page);
    await use(page);
    await stopAndWriteCoverage(page, testInfo.titlePath.join(" > "), started);
  },
});

export { expect };
