// Regenerates docs/assets/{web,acp} screenshots from seeded servers (not a behavior test).
// Run via scripts/dev/capture-web-screenshots.sh or playwright.capture.config.ts. Fixed viewports, reduced
// motion, and seeded data keep the PNGs stable.

import { spawnSync } from "node:child_process";
import { mkdirSync, mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";
import { test as base, type Page } from "@playwright/test";
import { spawnAoeServe, listSessions, resolveAoeBinary, seedSessionViaAoeAdd } from "../helpers/aoeServe";
import { commitAll, initWorkingRepo, writeFiles } from "../helpers/gitFixture";
import { waitForStructuredView, enableStructuredViewAndWait } from "../helpers/acp";

const HERE = dirname(fileURLToPath(import.meta.url));
const REPO_ROOT = resolve(HERE, "..", "..", "..");
const ASSETS = join(REPO_ROOT, "docs", "assets");
const DESKTOP = { width: 1440, height: 900 };
const MOBILE = { width: 390, height: 844 };

async function shot(page: Page, rel: string): Promise<void> {
  const out = join(ASSETS, rel);
  mkdirSync(join(out, ".."), { recursive: true });
  await page.waitForTimeout(700);
  await page.screenshot({ path: out });
  console.log(`captured ${rel}`);
}

base("web dashboard surfaces", async ({ page }, testInfo) => {
  const serve = await spawnAoeServe({
    authMode: "none",
    workerIndex: testInfo.workerIndex,
    parallelIndex: testInfo.parallelIndex,
    seedFn: ({ home, env }) => {
      const add = (dir: string, title: string) => {
        const res = spawnSync(resolveAoeBinary(), ["add", dir, "-t", title, "-c", "claude"], { env });
        if (res.status !== 0) {
          throw new Error(`aoe add ${title} failed: ${res.stderr?.toString() ?? "<none>"}`);
        }
      };
      for (const [sub, title] of [
        ["auth-service", "auth-service"],
        ["web-frontend", "web-frontend"],
      ] as const) {
        const dir = join(home, sub);
        initWorkingRepo(dir, env);
        writeFiles(dir, { "README.md": `# ${title}\n` });
        commitAll(dir, "init", env);
        add(dir, title);
      }
      const apiDir = join(home, "api-server");
      initWorkingRepo(apiDir, env);
      writeFiles(apiDir, {
        "src/routes.ts": "export const routes = [];\n",
        "src/auth.ts": "export function login() {}\n",
        "README.md": "# api-server\n",
      });
      commitAll(apiDir, "baseline", env);
      writeFiles(apiDir, {
        "src/routes.ts": "export const routes = [\n  { path: '/health', handler: health },\n];\n",
        "src/auth.ts": "export function login(user: string) {\n  return issueToken(user);\n}\n",
        "README.md": "# api-server\n\nNow with auth and health routes.\n",
      });
      add(apiDir, "api-server");
    },
  });

  try {
    await page.emulateMedia({ reducedMotion: "reduce" });
    await page.setViewportSize(DESKTOP);

    await page.goto(`${serve.baseUrl}/`);
    await page.getByRole("link").filter({ hasText: "api-server" }).first().waitFor({ timeout: 15_000 });
    await shot(page, "web/dashboard.png");

    const sessions = await listSessions(serve.baseUrl);
    const api = sessions.find((s) => s.title === "api-server");
    if (!api) throw new Error("seeded session 'api-server' missing");

    await page.goto(`${serve.baseUrl}/session/${encodeURIComponent(api.id)}`);
    await page.locator(".xterm").first().waitFor({ timeout: 15_000 });
    await shot(page, "web/terminal.png");

    await page
      .getByText("3 files", { exact: true })
      .first()
      .waitFor({ timeout: 15_000 })
      .catch(() => {});
    const fileRow = page.getByText("auth.ts", { exact: true }).first();
    if (await fileRow.isVisible().catch(() => false)) {
      await fileRow.click();
      await page
        .getByText(/issueToken/)
        .first()
        .waitFor({ timeout: 10_000 })
        .catch(() => {});
      await page.waitForTimeout(400);
    }
    await shot(page, "web/diff.png");

    await page.goto(`${serve.baseUrl}/settings`);
    await page.waitForTimeout(600);
    await shot(page, "web/settings.png");
  } finally {
    await serve.stop();
  }
});

const ACP_SCRIPT = {
  turns: [
    {
      updates: [
        {
          sessionUpdate: "plan",
          entries: [
            {
              content: "Add a /health route",
              status: "completed",
              priority: "high",
            },
            {
              content: "Wire auth into login()",
              status: "in_progress",
              priority: "high",
            },
            {
              content: "Add a regression test",
              status: "pending",
              priority: "medium",
            },
          ],
        },
        {
          sessionUpdate: "agent_message_chunk",
          content: {
            type: "text",
            text: "I'll wire authentication into the login handler and add a health route.",
          },
        },
        {
          sessionUpdate: "tool_call",
          toolCallId: "tc-read-1",
          title: "read src/auth.ts",
          kind: "read",
          status: "completed",
          rawInput: { file_path: "src/auth.ts" },
        },
        {
          sessionUpdate: "tool_call",
          toolCallId: "tc-edit-1",
          title: "edit src/auth.ts",
          kind: "edit",
          status: "completed",
          rawInput: {
            file_path: "src/auth.ts",
            old_string: "export function login() {}",
            new_string: "export function login(user: string) {\n  return issueToken(user);\n}",
          },
        },
        {
          sessionUpdate: "tool_call",
          toolCallId: "tc-bash-1",
          title: "npm test",
          kind: "execute",
          status: "completed",
          rawInput: { command: "npm test" },
        },
        {
          sessionUpdate: "agent_message_chunk",
          content: {
            type: "text",
            text: "Done. Auth is wired in and the health route is live; tests pass.",
          },
        },
      ],
      stopReason: "end_turn",
    },
  ],
};

base("structured view surfaces", async ({ page }, testInfo) => {
  const scriptDir = mkdtempSync(join(tmpdir(), "aoe-cap-acp-"));
  const scriptPath = join(scriptDir, "script.json");
  writeFileSync(scriptPath, JSON.stringify(ACP_SCRIPT));

  const serve = await spawnAoeServe({
    authMode: "none",
    acp: true,
    fakeAcpScript: scriptPath,
    workerIndex: testInfo.workerIndex,
    parallelIndex: testInfo.parallelIndex,
    seedFn: seedSessionViaAoeAdd({ title: "wire-auth" }),
  });

  try {
    await page.emulateMedia({ reducedMotion: "reduce" });
    await page.setViewportSize(DESKTOP);

    const sessions = await listSessions(serve.baseUrl);
    const seeded = sessions.find((s) => s.title === "wire-auth");
    if (!seeded) throw new Error("seeded session 'wire-auth' missing");
    await enableStructuredViewAndWait(serve.baseUrl, seeded.id);

    await page.goto(`${serve.baseUrl}/session/${encodeURIComponent(seeded.id)}`);
    await waitForStructuredView(page);

    const composer = page.getByRole("textbox", {
      name: /Send a message|Queue a follow-up/i,
    });
    await composer.fill("Wire auth into login() and add a health route.");
    await composer.press("Enter");

    await page
      .getByText(/tests pass/i)
      .first()
      .waitFor({ timeout: 20_000 });
    await shot(page, "structured view/overview.png");

    // Reload at phone width so it mounts in mobile mode.
    await page.setViewportSize(MOBILE);
    await page.goto(`${serve.baseUrl}/session/${encodeURIComponent(seeded.id)}`);
    await waitForStructuredView(page);
    // Tap the backdrop to close the drawer.
    const sidebarHeading = page.getByTestId("sidebar-axis-heading");
    if (await sidebarHeading.isVisible()) {
      await page.mouse.click(340, 450);
      await sidebarHeading.waitFor({ state: "hidden", timeout: 5_000 });
    }
    await page.waitForTimeout(600);
    await shot(page, "structured view/interface.png");
  } finally {
    await serve.stop();
    rmSync(scriptDir, { recursive: true, force: true });
  }
});

const APPROVAL_SCRIPT = {
  turns: [
    {
      updates: [
        {
          sessionUpdate: "agent_message_chunk",
          content: {
            type: "text",
            text: "This will force-push to main. Confirm to proceed.",
          },
        },
        {
          sessionUpdate: "permission_request",
          toolCall: {
            toolCallId: "tc-approve-1",
            title: "git push --force origin main",
            kind: "execute",
            rawInput: { command: "git push --force origin main" },
          },
        },
        {
          sessionUpdate: "agent_message_chunk",
          content: { type: "text", text: "Pushed." },
        },
      ],
      stopReason: "end_turn",
    },
  ],
};

base("structured view approval card", async ({ page }, testInfo) => {
  const scriptDir = mkdtempSync(join(tmpdir(), "aoe-cap-approval-"));
  const scriptPath = join(scriptDir, "script.json");
  writeFileSync(scriptPath, JSON.stringify(APPROVAL_SCRIPT));

  const serve = await spawnAoeServe({
    authMode: "none",
    acp: true,
    fakeAcpScript: scriptPath,
    workerIndex: testInfo.workerIndex,
    parallelIndex: testInfo.parallelIndex,
    seedFn: seedSessionViaAoeAdd({ title: "approve-push" }),
  });

  try {
    await page.emulateMedia({ reducedMotion: "reduce" });
    await page.setViewportSize(DESKTOP);

    const sessions = await listSessions(serve.baseUrl);
    const seeded = sessions.find((s) => s.title === "approve-push");
    if (!seeded) throw new Error("seeded session 'approve-push' missing");
    await enableStructuredViewAndWait(serve.baseUrl, seeded.id);

    await page.goto(`${serve.baseUrl}/session/${encodeURIComponent(seeded.id)}`);
    await waitForStructuredView(page);

    const composer = page.getByRole("textbox", {
      name: /Send a message|Queue a follow-up/i,
    });
    await composer.fill("push my changes");
    await composer.press("Enter");

    await page
      .getByText(/git push --force/i)
      .first()
      .waitFor({ timeout: 20_000 });
    await page.waitForTimeout(400);
    await shot(page, "structured view/approval.png");
  } finally {
    await serve.stop();
    rmSync(scriptDir, { recursive: true, force: true });
  }
});
