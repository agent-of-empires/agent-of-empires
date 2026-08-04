// Live: the skills settings panel (#3050) discovers a real external skill,
// adopts it without changing the source, and persists managed edits/deletion.

import { test, expect } from "@playwright/test";
import { mkdirSync, readFileSync, writeFileSync } from "node:fs";
import { join } from "node:path";
import { spawnAoeServe } from "../helpers/aoeServe";

test("skills panel adopts, edits, creates, and deletes skills", async ({ page }, testInfo) => {
  const original = "---\nname: Review\ndescription: Review code carefully\n---\n\nOriginal body\n";
  const serve = await spawnAoeServe({
    authMode: "none",
    workerIndex: testInfo.workerIndex,
    parallelIndex: testInfo.parallelIndex,
    seedFn: ({ home }) => {
      const skill = join(home, ".claude", "skills", "review");
      mkdirSync(skill, { recursive: true });
      writeFileSync(join(skill, "SKILL.md"), original);
    },
  });

  try {
    await page.goto(`${serve.baseUrl}/settings/skills`);

    await expect(page.getByRole("heading", { name: "Skills Library" })).toBeVisible();
    // Anchored: the detail pane's "Preview" toggle also contains "review", so
    // an unanchored match is ambiguous and trips strict mode.
    await page.getByRole("button", { name: /^review/i }).click();
    await expect(page.getByText("claude-user").first()).toBeVisible();
    await expect(page.getByLabel("SKILL.md content")).toHaveAttribute("readonly", "");

    await page.getByRole("button", { name: "Adopt into AoE" }).click();
    await expect(page.getByRole("status")).toContainText("adopted");
    await expect(page.getByLabel("SKILL.md content")).not.toHaveAttribute("readonly");
    expect(readFileSync(join(serve.home, ".claude", "skills", "review", "SKILL.md"), "utf8")).toBe(original);

    const edited = "---\nname: Review\ndescription: Updated review\n---\n\nEdited body\n";
    await page.getByLabel("SKILL.md content").fill(edited);
    await page.getByRole("button", { name: "Save" }).click();
    await expect(page.getByRole("status")).toContainText("saved");

    const persisted = await page.request.get(`${serve.baseUrl}/api/skills/aoe-managed/review`);
    expect(persisted.ok()).toBe(true);
    expect((await persisted.json()).content).toBe(edited);

    await page.getByRole("button", { name: "+ New skill" }).click();
    await page.getByLabel("New skill directory").fill("new-skill");
    await page.getByLabel("New skill description").fill("Use for new work");
    await page.getByRole("button", { name: "Create" }).click();
    await expect(page.getByRole("status")).toContainText("created");
    await expect(page.getByLabel("SKILL.md content")).toHaveValue(/name: new-skill/);

    page.once("dialog", (dialog) => void dialog.accept());
    await page.getByRole("button", { name: "Delete" }).click();
    await expect(page.getByRole("status")).toContainText("deleted");
    const removed = await page.request.get(`${serve.baseUrl}/api/skills/aoe-managed/new-skill`);
    expect(removed.status()).toBe(404);
  } finally {
    await serve.stop();
  }
});
