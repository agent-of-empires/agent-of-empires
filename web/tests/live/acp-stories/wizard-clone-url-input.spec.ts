// User story: enter a URL on the Clone URL tab; the input accepts it
// and is ready to submit.
//
// Full clone is an HTTP round-trip outside test scope; this story
// covers the input plumbing only. The Clone button only enables when
// the URL is non-empty.

import { test as base, expect } from "@playwright/test";
import { spawnAoeServe } from "../../helpers/aoeServe";

base("wizard Clone URL tab accepts a repo URL", async ({ page }, testInfo) => {
  const serve = await spawnAoeServe({
    authMode: "none",
    workerIndex: testInfo.workerIndex,
    parallelIndex: testInfo.parallelIndex,
  });

  try {
    await page.goto(serve.baseUrl);
    await page.getByRole("button", { name: "New session", exact: true }).first().click();
    await page.getByRole("button", { name: "Clone URL", exact: true }).click();

    const input = page.getByPlaceholder("https://github.com/user/repo.git");
    const cloneButton = page.getByRole("button", {
      name: /^Clone repository$/i,
    });
    await expect(input).toBeVisible({ timeout: 5_000 });
    await expect(cloneButton).toBeDisabled({ timeout: 5_000 });
    await input.fill("https://github.com/example/repo.git");
    await expect(input).toHaveValue("https://github.com/example/repo.git");
    await expect(cloneButton).toBeEnabled({ timeout: 5_000 });
  } finally {
    await serve.stop();
  }
});
