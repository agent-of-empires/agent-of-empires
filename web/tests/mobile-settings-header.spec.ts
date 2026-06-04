import { test, expect } from "./helpers/mockedTest";
import { devices } from "@playwright/test";

// Anti-regression for #1430. The old header crammed Back + "Settings" title
// + ProfileSelector into one h-12 row; ProfileSelector was wrapped in a
// `flex-1 justify-center` div that squeezed the back affordance and title
// down to the left edge at mobile widths. The fix makes the header use
// flex-wrap so the ProfileSelector drops onto a second row below md, and
// keeps a single-row layout (selector on the right) on md+.

test.use({ ...devices["iPhone 13"] });

test.describe("Mobile settings header (iPhone 13)", () => {
  test("Back button and title sit on a row above the ProfileSelector", async ({ page }) => {
    await page.goto("/settings");

    const backBtn = page.getByRole("button", { name: /Back/ });
    const profileLabel = page.getByText("Profile", { exact: true });

    await expect(backBtn).toBeVisible();
    await expect(profileLabel).toBeVisible();

    const backBox = await backBtn.boundingBox();
    const profileBox = await profileLabel.boundingBox();
    expect(backBox).not.toBeNull();
    expect(profileBox).not.toBeNull();
    // ProfileSelector must wrap onto its own row, i.e. start at or below
    // the bottom of the Back button (mobile two-row layout).
    expect(profileBox!.y).toBeGreaterThanOrEqual(backBox!.y + backBox!.height - 1);
  });

  test("'Settings' title keeps clear separation from the Back button", async ({ page }) => {
    await page.goto("/settings");
    const backBtn = page.getByRole("button", { name: /Back/ });
    const title = page.getByText("Settings", { exact: true }).first();
    const backBox = await backBtn.boundingBox();
    const titleBox = await title.boundingBox();
    expect(backBox).not.toBeNull();
    expect(titleBox).not.toBeNull();
    // gap-x-3 on the header (12px); allow a small fudge for sub-pixel
    // layout, anything above 6px proves the cramped-against-left-edge
    // regression is gone.
    expect(titleBox!.x - (backBox!.x + backBox!.width)).toBeGreaterThan(6);
  });

  test("settings header does not overflow horizontally at iPhone 13 width", async ({ page }) => {
    await page.goto("/settings");
    const header = page.getByTestId("settings-header");
    await expect(header).toBeVisible();
    const overflow = await header.evaluate((el) => ({
      scrollWidth: el.scrollWidth,
      clientWidth: el.clientWidth,
    }));
    expect(overflow.scrollWidth).toBeLessThanOrEqual(overflow.clientWidth);
  });
});

test.describe("Mobile settings header (narrow 320px)", () => {
  test.use({ viewport: { width: 320, height: 568 } });

  test("header stays within viewport at 320px (5.4\" Android narrow case)", async ({ page }) => {
    await page.goto("/settings");
    const header = page.getByTestId("settings-header");
    await expect(header).toBeVisible();
    const headerBox = await header.boundingBox();
    expect(headerBox).not.toBeNull();
    // Header element must fit within the viewport width. Overflow inside
    // the ProfileSelector row is allowed via overflow-x-auto, but the
    // header container itself must not push past the viewport edge.
    expect(headerBox!.width).toBeLessThanOrEqual(320);
  });
});
