import { chmodSync, readFileSync, statSync, writeFileSync } from "node:fs";
import { join } from "node:path";
import type { Dialog, Page } from "@playwright/test";
import { test, expect, type ServeHandle } from "../helpers/liveTest";
import { enableStructuredViewAndWait, composer } from "../helpers/acp";
import { listSessions, seedSessionViaAoeAdd } from "../helpers/aoeServe";

async function openFile(page: Page, serve: ServeHandle, title?: string) {
  const sessions = await listSessions(serve.baseUrl);
  const session = title ? sessions.find((item) => item.title === title) : sessions[0];
  const sessionLink = page.locator(`a[href="/session/${session!.id}"]`).first();
  // In-app navigation keeps one document, so a later history.back() stays a popstate.
  if (page.url().startsWith(serve.baseUrl)) await sessionLink.click();
  else await page.goto(`${serve.baseUrl}/session/${session!.id}`);
  await showNotes(page);
  return session!;
}

async function showNotes(page: Page) {
  if (page.viewportSize()!.width < 768) {
    await page.getByRole("button", { name: "Toggle panels" }).click();
    await page.getByTestId("mobile-right-panel-picker").getByRole("button", { name: "Files", exact: true }).click();
  } else {
    const toggle = page.getByRole("button", { name: "Toggle Files pane" }).first();
    await expect(toggle).toBeVisible();
    if ((await toggle.getAttribute("aria-pressed")) !== "true") await toggle.click();
  }
  await page.getByRole("button", { name: "notes.txt", exact: true }).first().click();
}

async function enableEditing(page: Page, serve: ServeHandle) {
  await page.goto(`${serve.baseUrl}/settings/panels`);
  const persisted = page.waitForResponse(
    (response) =>
      response.url().endsWith("/api/app-state/web-ui-state") &&
      response.request().method() === "PATCH" &&
      JSON.parse(response.request().postDataJSON()["aoe-web-settings"] ?? "{}").fileEditing === true,
  );
  await page.getByRole("checkbox", { name: "Edit project text files" }).check();
  await persisted;
  await page.getByTestId("settings-header").getByRole("button", { name: /Back/ }).click();
  await expect(page).toHaveURL(`${serve.baseUrl}/`);
}

test("file editing is opt-in, persists exact bytes and mode, and guards dirty pane navigation", async ({
  page,
  spawnServe,
}, testInfo) => {
  let target = "";
  const serve = await spawnServe({
    seedFn: (env) => {
      seedSessionViaAoeAdd({
        title: "file-edit",
        git: false,
        files: { "notes.txt": "first\r\nsecond\r\n" },
        prepare: (dir) => {
          target = join(dir, "notes.txt");
          chmodSync(target, 0o751);
        },
      })(env);
      seedSessionViaAoeAdd({
        title: "file-second",
        subdir: "second-project",
        git: false,
        files: { "notes.txt": "other file" },
      })(env);
    },
  });
  await openFile(page, serve, "file-edit");
  await expect(page.getByRole("button", { name: "Edit", exact: true })).toHaveCount(0);
  await enableEditing(page, serve);
  await openFile(page, serve, "file-edit");
  await page.getByRole("button", { name: "Edit", exact: true }).click();
  const editor = page.getByRole("textbox", { name: "Edit notes.txt" });
  await expect(editor).toBeFocused();
  await editor.fill("changed\nsecond\n");
  page.once("dialog", (dialog) => dialog.dismiss());
  await page.getByRole("button", { name: "Close files", exact: true }).click();
  await expect(editor).toHaveValue("changed\nsecond\n");
  page.once("dialog", (dialog) => dialog.dismiss());
  await page.getByRole("link").filter({ hasText: "file-second" }).click();
  await expect(editor).toHaveValue("changed\nsecond\n");
  // Selecting the already-active tab keeps the draft without a discard prompt.
  const unexpectedDialogs: string[] = [];
  const recordDialog = (dialog: Dialog) => {
    unexpectedDialogs.push(dialog.message());
    void dialog.dismiss();
  };
  page.on("dialog", recordDialog);
  await page.getByRole("tab", { name: "Files", exact: true }).click();
  page.off("dialog", recordDialog);
  expect(unexpectedDialogs).toEqual([]);
  await expect(editor).toHaveValue("changed\nsecond\n");
  page.once("dialog", (dialog) => dialog.dismiss());
  await page.getByRole("button", { name: "New terminal", exact: true }).first().click();
  await expect(editor).toHaveValue("changed\nsecond\n");
  const currentUrl = page.url();
  const leaving = page.waitForEvent("dialog");
  await page.evaluate(() => window.history.back());
  const discard = await leaving;
  expect(discard.type()).toBe("confirm");
  await discard.dismiss();
  await expect(page).toHaveURL(currentUrl);
  await expect(editor).toHaveValue("changed\nsecond\n");
  await editor.press("Control+s");
  await expect(page.getByText("File saved.", { exact: true })).toBeVisible();
  expect(readFileSync(target, "utf8")).toBe("changed\r\nsecond\r\n");
  expect(statSync(target).mode & 0o777).toBe(0o751);
  await expect(page.getByText(/Let the agent know you changed/)).toBeVisible();
  await page.screenshot({ path: testInfo.outputPath("file-saved.png") });
  await page.reload();
  // A reload restores the pane layout, while selection stays intentionally local.
  const notes = page.getByRole("button", { name: "notes.txt", exact: true }).first();
  await notes.click();
  await expect(page.getByRole("button", { name: "Edit", exact: true })).toBeEnabled();
});

test("external writes show a live warning and stale HTTP saves preserve the agent's text", async ({
  page,
  spawnServe,
}, testInfo) => {
  let target = "";
  const serve = await spawnServe({
    seedFn: seedSessionViaAoeAdd({
      title: "file-conflict",
      git: false,
      files: { "notes.txt": "original\n" },
      prepare: (dir) => {
        target = join(dir, "notes.txt");
      },
    }),
  });
  await enableEditing(page, serve);
  const session = await openFile(page, serve);
  const opened = await (
    await page.request.get(`${serve.baseUrl}/api/sessions/${session.id}/file?path=notes.txt&editing=true`)
  ).json();
  await page.getByRole("button", { name: "Edit", exact: true }).click();
  const editor = page.getByRole("textbox", { name: "Edit notes.txt" });
  await editor.fill("my unsaved edit\n");
  writeFileSync(target, "agent update\n");
  await expect(page.getByText("File changed on disk. Copy your edits or reload before saving.")).toBeVisible({
    timeout: 10_000,
  });
  await expect(editor).toHaveValue("my unsaved edit\n");
  await expect(page.getByRole("button", { name: "Save", exact: true })).toBeDisabled();
  const save = await page.request.put(`${serve.baseUrl}/api/sessions/${session.id}/file`, {
    data: { path: "notes.txt", version: opened.version, content: "overwrite" },
  });
  expect(save.status()).toBe(409);
  expect(readFileSync(target, "utf8")).toBe("agent update\n");
  await page.screenshot({ path: testInfo.outputPath("save-conflict.png") });
  page.once("dialog", (dialog) => dialog.accept());
  await page.getByRole("button", { name: "Reload from disk" }).click();
  await expect(editor).toHaveCount(0);
  await expect(page.getByText("File saved.", { exact: true })).toHaveCount(0);
  await page.getByRole("button", { name: "Edit", exact: true }).click();
  await expect(page.getByRole("textbox", { name: "Edit notes.txt" })).toHaveValue("agent update\n");
});

test("read-only mode refuses editing even with the browser preference enabled", async ({ page, spawnServe }) => {
  const serve = await spawnServe({
    readOnly: true,
    seedFn: seedSessionViaAoeAdd({ title: "file-readonly", git: false, files: { "notes.txt": "original" } }),
  });
  await page.addInitScript(() => localStorage.setItem("aoe-web-settings", JSON.stringify({ fileEditing: true })));
  const session = await openFile(page, serve);
  await expect(page.getByRole("button", { name: "Edit", exact: true })).toBeDisabled();
  await expect(page.getByText("The dashboard is read-only", { exact: true })).toBeVisible();
  const result = await page.request.put(`${serve.baseUrl}/api/sessions/${session.id}/file`, {
    data: { path: "notes.txt", content: "overwrite", version: "x" },
  });
  expect(result.status()).toBe(403);
});

test("mobile editing keeps controls usable and confirms leaving the Files pane", async ({
  page,
  spawnServe,
}, testInfo) => {
  await page.setViewportSize({ width: 390, height: 844 });
  const serve = await spawnServe({
    seedFn: seedSessionViaAoeAdd({ title: "file-mobile", git: false, files: { "notes.txt": "original" } }),
  });
  await page.addInitScript(() =>
    localStorage.setItem("aoe-web-settings", JSON.stringify({ fileEditing: true, autoOpenKeyboard: false })),
  );
  await openFile(page, serve);
  await page.getByRole("button", { name: "Edit", exact: true }).click();
  const editor = page.getByRole("textbox", { name: "Edit notes.txt" });
  await editor.fill("phone edit");
  await page.screenshot({ path: testInfo.outputPath("mobile-editor.png") });
  page.once("dialog", (dialog) => dialog.dismiss());
  await page.getByTestId("mobile-back-to-agent").click();
  await expect(editor).toHaveValue("phone edit");
  page.once("dialog", (dialog) => dialog.accept());
  await editor.press("Escape");
  await expect(editor).toHaveCount(0);
});

test("an unsaved draft survives crossing the mobile breakpoint in both directions", async ({ page, spawnServe }) => {
  const serve = await spawnServe({
    seedFn: seedSessionViaAoeAdd({ title: "file-resize", git: false, files: { "notes.txt": "original" } }),
  });
  await page.addInitScript(() =>
    localStorage.setItem("aoe-web-settings", JSON.stringify({ fileEditing: true, autoOpenKeyboard: false })),
  );
  await openFile(page, serve);
  await page.getByRole("button", { name: "Edit", exact: true }).click();
  const editor = page.getByRole("textbox", { name: "Edit notes.txt" });
  await editor.fill("desktop draft");
  await page.setViewportSize({ width: 390, height: 844 });
  // The desktop sidebar stays open as an overlay after the resize.
  await page.getByRole("navigation", { name: "Sessions sidebar" }).getByRole("button", { name: "×" }).click();
  // The Files pane keeps its selection locally; reopening the file restores the draft.
  await showNotes(page);
  await expect(editor).toHaveValue("desktop draft");
  await editor.fill("phone draft");
  await page.setViewportSize({ width: 1280, height: 720 });
  await showNotes(page);
  await expect(editor).toHaveValue("phone draft");
});

test("a Structured view save offers a note that appends to the draft without submitting", async ({
  page,
  spawnServe,
}, testInfo) => {
  let target = "";
  const serve = await spawnServe({
    acp: true,
    seedFn: seedSessionViaAoeAdd({
      title: "file-structured",
      git: false,
      files: { "notes.txt": "original" },
      prepare: (dir) => {
        target = join(dir, "notes.txt");
      },
    }),
  });
  const [session] = await listSessions(serve.baseUrl);
  await enableStructuredViewAndWait(serve.baseUrl, session!.id, 30_000, serve.home);
  await enableEditing(page, serve);
  await openFile(page, serve);
  const draft = composer(page);
  await expect(draft).toBeVisible();
  await draft.fill("existing prompt draft");
  await page.getByRole("button", { name: "Edit", exact: true }).click();
  await page.getByRole("textbox", { name: "Edit notes.txt" }).fill("changed in browser");
  await page.getByRole("button", { name: "Save", exact: true }).click();
  const notify = page.getByRole("button", { name: "Draft a note to the agent" });
  await expect(notify).toBeVisible();
  expect(readFileSync(target, "utf8")).toBe("changed in browser");
  const before = await (
    await page.request.get(`${serve.baseUrl}/api/sessions/${session!.id}/acp/replay?view=rows`)
  ).json();
  await notify.click();
  await expect(draft).toHaveValue(
    "existing prompt draft\n\nI edited notes.txt in the file viewer. Please review the changes before continuing.",
  );
  const after = await (
    await page.request.get(`${serve.baseUrl}/api/sessions/${session!.id}/acp/replay?view=rows`)
  ).json();
  expect(after.highest_seq).toBe(before.highest_seq);
  await page.screenshot({ path: testInfo.outputPath("structured-notice.png") });
});
