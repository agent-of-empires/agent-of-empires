import { test, expect } from "./helpers/mockedTest";
import { mockStructuredSessionApis } from "./helpers/structuredSessionMocks";
import { Page } from "@playwright/test";
import { clickSidebarSession } from "./helpers/sidebar";

// #1768: the Edit tool card's `+N -M` chip and expandable body are driven by
// `diffPair`, which only ever executes in a browser through this card. One
// `ToolCallStarted` edit frame over the structured-view WebSocket renders the
// card, runs `diffPair`, and mounts `StringDiff` on expand.

const SESSION_ID = "sess-1";
const FILE_PATH = "src/example.ts";

// old != new with one changed line per side plus a pure addition, so
// diffPair takes the parseDiffFromFile path and emits +3 / −2.
const OLD_STRING = "const x = 42;\nconst y = 1;\nexport default x;";
const NEW_STRING = "const x: number = 42;\nconst y = 2;\nconst z = 3;\nexport default x;";

const EDIT_TOOL_CALL = {
  id: "tc-1",
  name: "Edit",
  kind: "edit",
  args_preview: JSON.stringify({
    file_path: FILE_PATH,
    old_string: OLD_STRING,
    new_string: NEW_STRING,
  }),
  started_at: new Date().toISOString(),
};

/** The daemon's folded `tool_start` row for that call (src/acp/transcript.rs).
 *  The transcript is server-owned, so the card renders from this row, not from
 *  the raw `ToolCallStarted` frame. */
function editRowDelta() {
  return {
    kind: "transcript_delta",
    delta: {
      Append: {
        id: `start-${EDIT_TOOL_CALL.id}`,
        group_id: `tool-${EDIT_TOOL_CALL.id}`,
        kind: "tool_start",
        at: EDIT_TOOL_CALL.started_at,
        text: EDIT_TOOL_CALL.name,
        tool_call_id: EDIT_TOOL_CALL.id,
        tool: EDIT_TOOL_CALL,
      },
    },
  };
}

async function setup(page: Page) {
  await mockStructuredSessionApis(page, { id: SESSION_ID, title: "acp-edit-card" });
  // Push the server-folded edit row so the card renders; registered after the
  // helper's silent socket so it wins Playwright's reverse-order matching.
  await page.routeWebSocket(/\/sessions\/[^/]+\/acp\/ws/, (ws) => {
    ws.send(JSON.stringify(editRowDelta()));
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
