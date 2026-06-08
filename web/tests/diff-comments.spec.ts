import { test, expect } from "./helpers/mockedTest";
import { Page } from "@playwright/test";
import { clickSidebarSession } from "./helpers/sidebar";
import { makePatch } from "./helpers/patch";

// In-diff comments end-to-end (#928), against the @pierre/diffs renderer.
// - Structured-view-only feature: a session without the structured view
//   can't select lines to comment.
// - Select a line (click its gutter number) to comment; save; card renders.
// - Open the send dialog, edit intro, send; comments clear; POST
//   reaches /acp/prompt/diff-comments with the structured body
//   (intro/outro/comments/isMultiRepo/assembledMarkdown). See #1123.
// - Comments persist to localStorage and reload back into the UI.

const FILE_PATH = "src/example.ts";

const DIFF_FILES_RESPONSE = {
  files: [
    {
      path: FILE_PATH,
      old_path: null,
      status: "modified",
      additions: 3,
      deletions: 1,
    },
  ],
  per_repo_bases: [{ base_branch: "main" }],
  warning: null,
};

// Contents shape consumed by the @pierre/diffs renderer. The new-side line
// numbers below line up with the comment assertions (new line 3 =
// `function greet`, new line 4 = `return ...`).
const DIFF_FILE_RESPONSE = {
  file: {
    path: FILE_PATH,
    old_path: null,
    status: "modified",
    additions: 3,
    deletions: 1,
  },
  old_content:
    'import { useState } from "react";\nconst x = 42;\nexport default x;\n',
  new_content:
    'import { useState } from "react";\n' +
    "const x: number = 42;\n" +
    "function greet(name: string): string {\n" +
    "  return `Hello, ${name}`;\n" +
    "export default x;\n",
  is_binary: false,
  truncated: false,
};
// Server-computed patch, generated from the same contents.
(DIFF_FILE_RESPONSE as { patch?: string }).patch = makePatch(
  FILE_PATH,
  DIFF_FILE_RESPONSE.old_content,
  DIFF_FILE_RESPONSE.new_content,
);

interface SetupOpts {
  structuredView?: boolean;
  acpWorkerState?: "absent" | "resuming" | "running";
}

async function setup(page: Page, opts: SetupOpts = {}) {
  const structuredView = opts.structuredView ?? true;
  const acpWorkerState = opts.acpWorkerState ?? "running";
  await page.route("**/api/login/status", (r) =>
    r.fulfill({ json: { required: false, authenticated: true } }),
  );
  for (const path of [
    "settings",
    "themes",
    "agents",
    "profiles",
    "groups",
    "devices",
    "docker/status",
    "about",
    "system/update-status",
  ]) {
    await page.route(`**/api/${path}`, (r) =>
      r.fulfill({
        json:
          path === "docker/status" ||
          path === "about" ||
          path === "settings" ||
          path === "system/update-status"
            ? {}
            : [],
      }),
    );
  }
  await page.route("**/api/sessions", (r) => {
    if (r.request().method() === "POST") return r.fulfill({ status: 400 });
    return r.fulfill({
      json: {
        sessions: [
          {
            id: "sess-1",
            title: "diff-comments-test",
            project_path: "/tmp/diff-comments-test",
            group_path: "/tmp",
            tool: "claude",
            status: "Running",
            yolo_mode: false,
            created_at: new Date().toISOString(),
            last_accessed_at: null,
            last_error: null,
            branch: null,
            main_repo_path: null,
            is_sandboxed: false,
            has_terminal: true,
            profile: "default",
            workspace_repos: [],
            view: structuredView ? "structured" : "terminal",
            acp_worker_state: acpWorkerState,
            claude_fullscreen: false,
          },
        ],
        workspace_ordering: [],
      },
    });
  });
  await page.route("**/api/sessions/*/ensure", (r) =>
    r.fulfill({ json: { ok: true } }),
  );
  await page.route("**/api/sessions/*/terminal", (r) =>
    r.fulfill({ status: 200, body: "" }),
  );
  await page.route("**/api/sessions/*/diff/files", (r) =>
    r.fulfill({ json: DIFF_FILES_RESPONSE }),
  );
  await page.route(/\/api\/sessions\/[^/]+\/diff\/file\?/, (r) =>
    r.fulfill({ json: DIFF_FILE_RESPONSE }),
  );
  // Structured view panel endpoints — content irrelevant for these tests.
  await page.route("**/api/sessions/*/acp/**", (r) => r.fulfill({ json: {} }));
  await page.routeWebSocket(/\/sessions\/.*\/(ws|acp-ws)$/, () => {
    // No-op: we don't need a working stream for diff comment tests.
  });
}

async function openSessionAndFile(page: Page) {
  await page.goto("/");
  await expect(page.locator("header")).toBeVisible();
  await clickSidebarSession(page, "diff-comments-test");
  await expect(page.getByText("example.ts").first()).toBeVisible({
    timeout: 10000,
  });
  await page.getByText("example.ts").first().click();
  await expect(page.getByText("import { useState }").first()).toBeVisible({
    timeout: 10000,
  });
}

/** Click the @pierre/diffs gutter line-number cell for `lineNum`. The
 *  renderer lives in a shadow root; Playwright locators pierce it. A single
 *  click selects that line and fires onLineSelected, which opens the draft
 *  comment form. `[data-line-number-content]` cells contain only the number,
 *  so an exact-text filter is unambiguous. */
function gutterLine(page: Page, lineNum: number) {
  return page
    .locator("[data-line-number-content]")
    .filter({ hasText: new RegExp(`^${lineNum}$`) });
}

/** Open a single-line comment form by selecting one line. */
async function startSingleLineComment(page: Page, lineNum: number) {
  await gutterLine(page, lineNum).first().click();
}

/** Select a multi-line range: click the first line, shift-click the last. */
async function selectRange(page: Page, startLine: number, endLine: number) {
  await gutterLine(page, startLine).first().click();
  await gutterLine(page, endLine)
    .first()
    .click({ modifiers: ["Shift"] });
}

test.use({ viewport: { width: 1280, height: 900 } });

test.describe("Diff comments (#928)", () => {
  test("saves a single-line comment and renders the card inline", async ({
    page,
  }) => {
    await setup(page);
    await openSessionAndFile(page);
    await startSingleLineComment(page, 3);
    const textarea = page.getByPlaceholder(
      /Leave a comment \(markdown supported\)/,
    );
    await expect(textarea).toBeVisible();
    await textarea.fill("rename `greet` to `salute`");
    await page.getByRole("button", { name: "Save" }).click();
    await expect(textarea).toHaveCount(0);
    await expect(page.getByText("line 3 (new)").first()).toBeVisible();
    await expect(page.getByText("rename").first()).toBeVisible();
  });

  test("range select across two lines in the same hunk", async ({ page }) => {
    await setup(page);
    await openSessionAndFile(page);
    await selectRange(page, 3, 4);
    const textarea = page.getByPlaceholder(
      /Leave a comment \(markdown supported\)/,
    );
    await expect(textarea).toBeVisible();
    // Form heading should reflect the range
    await expect(page.getByText("lines 3-4 (new)").first()).toBeVisible();
    await textarea.fill("fix the function body");
    await page.getByRole("button", { name: "Save" }).click();
    await expect(page.getByText("lines 3-4 (new)").first()).toBeVisible();
  });

  test("banner shows count and persists comments through reload", async ({
    page,
  }) => {
    await setup(page);
    await openSessionAndFile(page);
    await startSingleLineComment(page, 3);
    await page
      .getByPlaceholder(/Leave a comment \(markdown supported\)/)
      .fill("nit");
    await page.getByRole("button", { name: "Save" }).click();
    await expect(page.getByText(/^1 comment$/).first()).toBeVisible();
    // (Banner renders once per visible right-pane instance; on desktop
    // both the standard and the resizing layout mount it, so `.first()`
    // is the cleanest way to assert presence rather than count.)

    // Reload and confirm the comment came back from localStorage.
    await page.reload();
    await expect(page.locator("header")).toBeVisible();
    await clickSidebarSession(page, "diff-comments-test");
    await expect(page.getByText(/^1 comment$/).first()).toBeVisible();
    await page.getByText("example.ts").first().click();
    await expect(page.getByText("nit").first()).toBeVisible();
  });

  test("send dialog POSTs structured body to /acp/prompt/diff-comments and clears comments on success", async ({
    page,
  }) => {
    await setup(page);
    interface CapturedBody {
      intro?: string;
      outro?: string;
      isMultiRepo?: boolean;
      comments?: Array<{ body?: string }>;
      assembledMarkdown?: string;
    }
    let captured: CapturedBody | null = null;
    await page.route("**/api/sessions/*/acp/prompt/diff-comments", (r) => {
      captured = JSON.parse(r.request().postData() || "{}");
      return r.fulfill({ json: {} });
    });
    // Capture the usage-signal pings so we can assert `diff_comments` fires
    // on a confirmed send (#1881).
    const seenSignals: string[] = [];
    await page.route("**/api/telemetry/seen", (r) => {
      try {
        const body = JSON.parse(r.request().postData() || "{}");
        if (typeof body.surface === "string") seenSignals.push(body.surface);
      } catch {
        // Ignore unparseable bodies.
      }
      return r.fulfill({ status: 204, body: "" });
    });
    await openSessionAndFile(page);
    await startSingleLineComment(page, 3);
    await page
      .getByPlaceholder(/Leave a comment \(markdown supported\)/)
      .fill("**rename** this please");
    await page.getByRole("button", { name: "Save" }).click();
    // Open the send dialog via the banner's Send button.
    await page
      .getByRole("button", { name: /^Send$/ })
      .first()
      .click();
    // Dialog open: heading "Send diff comments"
    await expect(page.getByText("Send diff comments")).toBeVisible();
    await page.getByPlaceholder(/Anything you want to say/).fill("Hey:");
    // Confirm send (dialog's own Send button is the last one in the DOM).
    await page
      .getByRole("button", { name: /^Send$/ })
      .last()
      .click();
    await expect.poll(() => captured?.assembledMarkdown).toBeTruthy();
    // Structured fields the transcript card renders from.
    expect(captured?.intro).toBe("Hey:");
    expect(captured?.outro).toBe("Please address these comments.");
    expect(captured?.isMultiRepo).toBe(false);
    expect(captured?.comments).toHaveLength(1);
    expect(captured?.comments?.[0]?.body).toContain("rename");
    // assembledMarkdown is the agent-visible body, no base64 sentinel.
    expect(captured?.assembledMarkdown).toContain("Hey:");
    expect(captured?.assembledMarkdown).toContain("## Diff comments");
    expect(captured?.assembledMarkdown).toContain("rename");
    expect(captured?.assembledMarkdown).toContain(
      "Please address these comments.",
    );
    expect(captured?.assembledMarkdown).not.toContain("aoe:diff-comments");
    // Banner cleared.
    await expect(page.getByText(/^1 comment$/)).toHaveCount(0);
    // The confirmed send fired the diff_comments usage signal (#1881).
    await expect.poll(() => seenSignals).toContain("diff_comments");
  });

  test("hides feature for non-structured view sessions", async ({ page }) => {
    await setup(page, { structuredView: false });
    await openSessionAndFile(page);
    // Line selection is disabled for tmux sessions, so selecting a line must
    // not open a comment form.
    await startSingleLineComment(page, 3);
    await expect(
      page.getByPlaceholder(/Leave a comment \(markdown supported\)/),
    ).toHaveCount(0);
  });

  test("send button disabled when worker not running", async ({ page }) => {
    await setup(page, { structuredView: true, acpWorkerState: "absent" });
    await openSessionAndFile(page);
    await startSingleLineComment(page, 3);
    await page
      .getByPlaceholder(/Leave a comment \(markdown supported\)/)
      .fill("nit");
    await page.getByRole("button", { name: "Save" }).click();
    const send = page.getByRole("button", { name: /^Send$/ }).first();
    await expect(send).toBeDisabled();
  });
});
