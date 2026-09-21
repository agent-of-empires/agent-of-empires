// Shared mocks for the mocked diff specs: file-list and file-contents routes
// on top of the standard terminal session boot.

import { expect, type Page } from "@playwright/test";
import { clickSidebarSession } from "./sidebar";
import { makePatch } from "./patch";
import { mockTerminalApis } from "./terminal-mocks";

export interface DiffFileInput {
  path: string;
  status?: string;
  additions?: number;
  deletions?: number;
  repo_name?: string;
}

export interface RepoBase {
  repo_name?: string;
  base_branch: string;
}

function diffFile(f: DiffFileInput) {
  return {
    path: f.path,
    old_path: null,
    status: f.status ?? "modified",
    additions: f.additions ?? 3,
    deletions: f.deletions ?? 1,
    ...(f.repo_name ? { repo_name: f.repo_name } : {}),
  };
}

export function diffFilesResponse(files: DiffFileInput[], bases: RepoBase[] = [{ base_branch: "main" }]) {
  return { files: files.map(diffFile), per_repo_bases: bases, warning: null };
}

/** Contents response for a single file, with the unified patch the server would ship. */
export function diffFileResponse(f: DiffFileInput, oldContent: string, newContent: string, patch?: string) {
  return {
    file: diffFile(f),
    old_content: oldContent,
    new_content: newContent,
    patch: patch ?? makePatch(f.path, oldContent, newContent),
    is_binary: false,
    truncated: false,
  };
}

/** Route the file list. A function is called per request, for sequenced responses. */
export async function mockDiffFiles(page: Page, json: object | ((call: number) => object)) {
  let calls = 0;
  await page.unroute("**/api/sessions/*/diff/files");
  await page.route("**/api/sessions/*/diff/files", (r) =>
    r.fulfill({ json: typeof json === "function" ? json(++calls) : json }),
  );
}

export async function mockDiffFileContents(page: Page, json: object) {
  await page.route(/\/api\/sessions\/[^/]+\/diff\/file\?/, (r) => r.fulfill({ json }));
}

/** Boot the terminal session mocks with a diff file list already routed. */
export async function setupDiffSession(
  page: Page,
  opts: {
    files?: object | ((call: number) => object);
    contents?: object;
    sessionFields?: Record<string, unknown>;
  } = {},
) {
  await mockTerminalApis(page, { sessionFields: opts.sessionFields });
  if (opts.files !== undefined) await mockDiffFiles(page, opts.files);
  if (opts.contents !== undefined) await mockDiffFileContents(page, opts.contents);
}

/** Load the app and open a session's changes panel. */
export async function openDiffSession(page: Page, title = "pinch-test") {
  await page.goto("/");
  await expect(page.locator("header")).toBeVisible();
  await clickSidebarSession(page, title);
}

/** Click a row in the file list once it has rendered. */
export async function openDiffFile(page: Page, name: string) {
  const row = page.getByText(name).first();
  await expect(row).toBeVisible({ timeout: 10000 });
  await row.click();
}
