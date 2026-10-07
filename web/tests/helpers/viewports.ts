// Device descriptors shared by the mocked specs.

import { devices } from "@playwright/test";

/** iPhone 13 minus `defaultBrowserType`, which `test.use` inside a describe forbids. */
export const iPhone13 = (({ defaultBrowserType: _browser, ...rest }) => rest)(devices["iPhone 13"]);

export const DESKTOP = { viewport: { width: 1280, height: 800 }, hasTouch: false };
