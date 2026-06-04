import { test, expect } from "./helpers/mockedTest";
import { Page } from "@playwright/test";
import { clickSidebarSession } from "./helpers/sidebar";

// In-diff comments end-to-end (#928).
// - Structured view-only feature: a non-structured view session must not show the
//   `+` gutter button or the banner.
// - Click `+` to comment on a single line; save; card renders.
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

const DIFF_FILE_RESPONSE = {
  file: {
    path: FILE_PATH,
    old_path: null,
    status: "modified",
    additions: 3,
    deletions: 1,
  },
  hunks: [
    {
      old_start: 1,
      old_lines: 3,
      new_start: 1,
      new_lines: 5,
      lines: [
        { type: "equal", old_line_num: 1, new_line_num: 1, content: 'import { useState } from "react";\n' },
        { type: "delete", old_line_num: 2, new_line_num: null, content: "const x = 42;\n" },
        { type: "add", old_line_num: null, new_line_num: 2, content: "const x: number = 42;\n" },
        { type: "add", old_line_num: null, new_line_num: 3, content: "function greet(name: string): string {\n" },
        { type: "add", old_line_num: null, new_line_num: 4, content: "  return `Hello, ${name}`;\n" },
        { type: "equal", old_line_num: 3, new_line_num: 5, content: "export default x;\n" },
      ],
    },
  ],
  is_binary: false,
  truncated: false,
};

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
          path === "docker/status" || path === "about" || path === "settings" || path === "system/update-status"
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
  await page.route("**/api/sessions/*/acp/**", (r) =>
    r.fulfill({ json: {} }),
  );
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

async function clickPlusOn(page: Page, side: "new" | "old", lineNum: number) {
  const btn = page.getByRole("button", {
    name: `Add comment on ${side} line ${lineNum}`,
  });
  // The button is opacity-0 until hover; click with `force` so we don't
  // depend on the hover transition firing in headless mode.
  await btn.click({ force: true });
}

/** Open a single-line comment form: click `+` twice on the same line. */
async function startSingleLineComment(
  page: Page,
  side: "new" | "old",
  lineNum: number,
) {
  await clickPlusOn(page, side, lineNum);
  await clickPlusOn(page, side, lineNum);
}

test.use({ viewport: { width: 1280, height: 900 } });

test.describe("Diff comments (#928)", () => {
  test("saves a single-line comment and renders the card inline", async ({
    page,
  }) => {
    await setup(page);
    await openSessionAndFile(page);
    await startSingleLineComment(page, "new", 3);
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
    await clickPlusOn(page, "new", 3);
    await clickPlusOn(page, "new", 4);
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
    await startSingleLineComment(page, "new", 3);
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
    await page.route(
      "**/api/sessions/*/acp/prompt/diff-comments",
      (r) => {
        captured = JSON.parse(r.request().postData() || "{}");
        return r.fulfill({ json: {} });
      },
    );
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
    await startSingleLineComment(page, "new", 3);
    await page
      .getByPlaceholder(/Leave a comment \(markdown supported\)/)
      .fill("**rename** this please");
    await page.getByRole("button", { name: "Save" }).click();
    // Open the send dialog via the banner's Send button.
    await page.getByRole("button", { name: /^Send$/ }).first().click();
    // Dialog open: heading "Send diff comments"
    await expect(page.getByText("Send diff comments")).toBeVisible();
    await page.getByPlaceholder(/Anything you want to say/).fill("Hey:");
    // Confirm send (dialog's own Send button is the last one in the DOM).
    await page.getByRole("button", { name: /^Send$/ }).last().click();
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
    // `+` button shouldn't render for tmux sessions.
    await expect(
      page.getByRole("button", { name: /Add comment on .* line/ }),
    ).toHaveCount(0);
  });

  test("send button disabled when worker not running", async ({ page }) => {
    await setup(page, { structuredView: true, acpWorkerState: "absent" });
    await openSessionAndFile(page);
    await startSingleLineComment(page, "new", 3);
    await page
      .getByPlaceholder(/Leave a comment \(markdown supported\)/)
      .fill("nit");
    await page.getByRole("button", { name: "Save" }).click();
    const send = page.getByRole("button", { name: /^Send$/ }).first();
    await expect(send).toBeDisabled();
  });
});
