// User story (#1512): picking a no-arg slash command from the structured view
// composer's `/` popover does not trap the next Enter.
//
// Before the fix in #1512, `insertSlashCommand` left the textarea as
// `/help` with no trailing whitespace. assistant-ui's `detectTrigger`
// scans backward from the cursor and halts on whitespace; without a
// trailing space the cursor sits inside the `/help` range and the
// popover immediately re-opens with the same item highlighted. The
// popover's keyboard handler claims Enter and Tab while it's open and
// runs `selectItem` instead of letting the keystroke reach the send
// path, so the user is stuck (no send, the popover keeps re-picking).
//
// The fix in `web/src/components/acp/Composer.tsx` is to always
// append a trailing space, regardless of `acceptsInput`. This spec
// drives the live structured view against a fake ACP agent that advertises a
// no-arg command, picks it via Enter, and asserts a single Enter
// thereafter sends the prompt.

import { test as base, expect } from "@playwright/test";
import {
  spawnAoeServe,
  listSessions,
  seedSessionViaAoeAdd,
} from "../../helpers/aoeServe";
import {
  waitForStructuredView,
  enableStructuredViewAndWait,
  waitForReplayContains,
} from "../../helpers/acp";

base(
  "picking a no-arg slash command does not trap Enter",
  async ({ page }, testInfo) => {
    const serve = await spawnAoeServe({
      authMode: "none",
      acp: true,
      workerIndex: testInfo.workerIndex,
      parallelIndex: testInfo.parallelIndex,
      seedFn: seedSessionViaAoeAdd({ title: "story-slash-pick-no-arg" }),
      extraEnv: {
        // Causes fakeAcpAgent.mjs to emit an available_commands_update
        // notification right after session/new, populating the slash
        // popover with one no-arg command (`/help`) and one args
        // command (`/review`). See web/tests/helpers/fakeAcpAgent.mjs.
        FAKE_ACP_COMMANDS: JSON.stringify([
          { name: "help", description: "Show help", accepts_input: false },
          {
            name: "review",
            description: "Review the diff",
            accepts_input: true,
            hint: "what to review",
          },
        ]),
      },
    });

    try {
      const sessions = await listSessions(serve.baseUrl);
      const seeded = sessions.find(
        (s) => s.title === "story-slash-pick-no-arg",
      );
      if (!seeded) {
        throw new Error("seeded session 'story-slash-pick-no-arg' missing");
      }
      const sessionId = seeded.id;
      await enableStructuredViewAndWait(serve.baseUrl, sessionId);
      // Explicit spawn so the supervisor's ACP session is attached
      // before the popover is driven. enable can race the implicit
      // spawn and the first available_commands_update would land before
      // any client is subscribed.
      const spawnRes = await fetch(
        `${serve.baseUrl}/api/sessions/${sessionId}/acp/spawn`,
        {
          method: "POST",
          headers: { "Content-Type": "application/json" },
          body: JSON.stringify({ agent: "claude" }),
        },
      );
      if (![200, 202, 409].includes(spawnRes.status)) {
        throw new Error(`structured view spawn failed: ${spawnRes.status}`);
      }

      // Block until the fake-ACP's available_commands_update has
      // reached the replay buffer. Without this, the browser can race
      // the supervisor handshake and arrive before the structured view
      // reducer has applied the commands, leaving the `/` popover
      // showing "No matches".
      await waitForReplayContains(
        serve.baseUrl,
        sessionId,
        "AvailableCommandsUpdated",
      );

      await page.goto(
        `${serve.baseUrl}/session/${encodeURIComponent(sessionId)}`,
      );
      await waitForStructuredView(page);

      const composer = page.getByRole("textbox", { name: /Send a message/i });
      await composer.click();

      // Type `/h` so the popover surfaces and `/help` is the top
      // (auto-highlighted index 0) item. fuzzyFilter ranks the prefix
      // match first.
      await composer.pressSequentially("/h");

      // assistant-ui renders each popover entry as a button with
      // role="option". The accessible name combines the trigger glyph,
      // the label, and the description, so filter by `/help` substring
      // rather than relying on the visually-only data-highlighted
      // attribute (which assistant-ui sets to "" not "true").
      const helpItem = page.getByRole("option").filter({ hasText: /\/help/ });
      await expect(helpItem).toBeVisible({ timeout: 15_000 });

      // Pick `/help` via Enter. removeOnExecute strips the typed
      // `/h`, then `insertSlashCommand` writes the canonical
      // `/help ` (with trailing space) back via setText.
      await composer.press("Enter");

      // Trailing space is the load-bearing fix. Without it,
      // detectTrigger's backward scan never finds whitespace and the
      // popover stays open.
      await expect(composer).toHaveValue("/help ", { timeout: 5_000 });

      // The popover must be closed at this point. If it is still open,
      // the next Enter will route into the popover's keyboard handler
      // and re-pick the highlighted item instead of sending.
      await expect(helpItem).toBeHidden({ timeout: 5_000 });

      // Single Enter should now route to the send path. The fake-ACP
      // default turn emits one agent_message_chunk with the canonical
      // "Hello from fake ACP agent." string.
      await composer.press("Enter");

      await expect(page.getByText("Hello from fake ACP agent.")).toBeVisible({
        timeout: 10_000,
      });
      // assistant-ui clears the composer asynchronously after the send
      // path resolves; give it a bounded window.
      await expect(composer).toHaveValue("", { timeout: 5_000 });
    } finally {
      await serve.stop();
    }
  },
);

base(
  "picking an args slash command leaves trailing space and closes popover",
  async ({ page }, testInfo) => {
    const serve = await spawnAoeServe({
      authMode: "none",
      acp: true,
      workerIndex: testInfo.workerIndex,
      parallelIndex: testInfo.parallelIndex,
      seedFn: seedSessionViaAoeAdd({ title: "story-slash-pick-args" }),
      extraEnv: {
        FAKE_ACP_COMMANDS: JSON.stringify([
          {
            name: "review",
            description: "Review the diff",
            accepts_input: true,
            hint: "what to review",
          },
        ]),
      },
    });

    try {
      const sessions = await listSessions(serve.baseUrl);
      const seeded = sessions.find((s) => s.title === "story-slash-pick-args");
      if (!seeded) {
        throw new Error("seeded session 'story-slash-pick-args' missing");
      }
      const sessionId = seeded.id;
      await enableStructuredViewAndWait(serve.baseUrl, sessionId);
      const spawnRes = await fetch(
        `${serve.baseUrl}/api/sessions/${sessionId}/acp/spawn`,
        {
          method: "POST",
          headers: { "Content-Type": "application/json" },
          body: JSON.stringify({ agent: "claude" }),
        },
      );
      if (![200, 202, 409].includes(spawnRes.status)) {
        throw new Error(`structured view spawn failed: ${spawnRes.status}`);
      }

      // Block until the fake-ACP's available_commands_update has
      // reached the replay buffer. Without this, the browser can race
      // the supervisor handshake and arrive before the structured view
      // reducer has applied the commands, leaving the `/` popover
      // showing "No matches".
      await waitForReplayContains(
        serve.baseUrl,
        sessionId,
        "AvailableCommandsUpdated",
      );

      await page.goto(
        `${serve.baseUrl}/session/${encodeURIComponent(sessionId)}`,
      );
      await waitForStructuredView(page);

      const composer = page.getByRole("textbox", { name: /Send a message/i });
      await composer.click();
      // Type `/rev` rather than `/r` so the fuzzy filter narrows to
      // `/review` alone. The structured view composer always seeds the claude
      // agent profile's `clearAliases` (`/clear`) into the popover
      // even when the agent doesn't advertise it (see
      // Composer.tsx:272-284), and a single `/r` matches `/clear` too;
      // `/clear` ranks ahead of `/review` and Enter would pick the
      // wrong command.
      await composer.pressSequentially("/rev");

      const reviewItem = page
        .getByRole("option")
        .filter({ hasText: /\/review/ });
      await expect(reviewItem).toBeVisible({ timeout: 15_000 });

      await composer.press("Enter");

      // The args branch was already correct before #1512 (its
      // trailing space served the cursor-positioning role). Lock that
      // contract: `/review ` plus closed popover, cursor ready for
      // arg typing.
      await expect(composer).toHaveValue("/review ", { timeout: 5_000 });
      await expect(reviewItem).toBeHidden({ timeout: 5_000 });

      // Typing afterward extends the existing token rather than
      // re-opening the popover.
      await composer.pressSequentially("scope");
      await expect(composer).toHaveValue("/review scope");
    } finally {
      await serve.stop();
    }
  },
);
