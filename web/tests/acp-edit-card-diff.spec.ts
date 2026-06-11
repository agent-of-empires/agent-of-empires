import { test, expect } from "./helpers/mockedTest";
import { Page } from "@playwright/test";
import { clickSidebarSession } from "./helpers/sidebar";

// Mocked render of the structured view Edit tool card (#1768).
//
// The card's `+N −M` chip and its expandable body are both driven by
// `diffPair` (web/src/lib/diffPair.ts), which runs an in-browser line
// diff over the tool's `(old_string, new_string)`. `diffPair` has full
// vitest coverage, but it is only ever *executed in a browser* through
// this card, so without a mocked-Playwright spec the istanbul build
// uploads it as 0/30 and codecov nets its patch coverage to ~0. This
// spec drives a single `ToolCallStarted` edit frame over the structured view
// WebSocket so the card renders, `diffPair` runs, and `StringDiff`
// mounts on expand.

const SESSION_ID = "sess-1";
const FILE_PATH = "src/example.ts";

// old != new with one changed line per side plus a pure addition, so
// diffPair takes the parseDiffFromFile path and emits +3 / −2.
const OLD_STRING = "const x = 42;\nconst y = 1;\nexport default x;";
const NEW_STRING = "const x: number = 42;\nconst y = 2;\nconst z = 3;\nexport default x;";

function editFrame() {
  return {
    session_id: SESSION_ID,
    seq: 1,
    event: {
      ToolCallStarted: {
        tool_call: {
          id: "tc-1",
          name: "Edit",
          kind: "edit",
          args_preview: JSON.stringify({
            file_path: FILE_PATH,
            old_string: OLD_STRING,
            new_string: NEW_STRING,
          }),
          started_at: new Date().toISOString(),
        },
      },
    },
  };
}

async function setup(page: Page) {
  await page.route("**/api/login/status", (r) => r.fulfill({ json: { required: false, authenticated: true } }));
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
            id: SESSION_ID,
            title: "acp-edit-card",
            project_path: "/tmp/acp-edit-card",
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
            view: "structured",
            acp_worker_state: "running",
            claude_fullscreen: false,
          },
        ],
        workspace_ordering: [],
      },
    });
  });
  await page.route("**/api/sessions/*/ensure", (r) => r.fulfill({ json: { ok: true } }));
  // Structured view REST endpoints (replay/snapshot/prompt): empty is fine, the
  // tool frame arrives over the WebSocket below.
  await page.route("**/api/sessions/*/acp/**", (r) => r.fulfill({ json: {} }));

  // Terminal WS (only opened outside structured view mode): swallow it.
  await page.routeWebSocket(/\/sessions\/[^/]+\/ws(\?|$)/, () => {
    // no-op
  });
  // Structured view WS: push one edit tool frame so the card renders.
  await page.routeWebSocket(/\/sessions\/[^/]+\/acp\/ws/, (ws) => {
    ws.send(JSON.stringify(editFrame()));
  });
}

test("structured view edit card renders diffPair output (chip + StringDiff)", async ({ page }) => {
  await setup(page);
  await page.goto("/");
  await expect(page.locator("header")).toBeVisible();
  await clickSidebarSession(page, "acp-edit-card");

  // The tool card header is a button labelled with the verb + file
  // path. Its presence proves the EditToolCard rendered, which means
  // diffPair ran in its useMemo to compute the chip.
  const card = page.getByRole("button").filter({ hasText: FILE_PATH }).first();
  await expect(card).toBeVisible({ timeout: 10000 });

  // diffPair tallied +3 / −2 for this pair; the chip surfaces them.
  await expect(card.getByText("+3")).toBeVisible();
  await expect(card.getByText("−2")).toBeVisible();

  // Expand to mount StringDiff, which runs diffPair again to build the
  // hunk and renders the added line.
  await card.click();
  const diff = page.getByTestId("string-diff");
  await expect(diff).toBeVisible({ timeout: 10000 });
  await expect(diff).toContainText("const z = 3;");
});
