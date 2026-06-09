// Live: the MCP servers settings panel (#1996) renders the effective set
// resolved from a real `aoe serve` backend, tags each server with its
// provenance, and never leaks a secret value to the DOM.

import { test as base, expect } from "@playwright/test";
import { writeFileSync } from "node:fs";
import { join } from "node:path";
import { spawnAoeServe } from "../helpers/aoeServe";

base("MCP panel shows native servers with provenance and redacts secrets", async ({ page }, testInfo) => {
  const serve = await spawnAoeServe({
    authMode: "none",
    workerIndex: testInfo.workerIndex,
    parallelIndex: testInfo.parallelIndex,
    // Seed the agent-native config (~/.claude.json) the resolver reads. It
    // carries a secret in both an env value and a header value, neither of
    // which may ever reach the rendered panel.
    seedFn: ({ home }) => {
      writeFileSync(
        join(home, ".claude.json"),
        JSON.stringify({
          mcpServers: {
            fs: {
              command: "mcp-fs",
              args: ["--root", "."],
              env: { TOKEN: "SUPER_SECRET_DO_NOT_LEAK" },
            },
            remote: {
              type: "http",
              url: "https://example/mcp",
              headers: { Authorization: "Bearer HEADER_SECRET_DO_NOT_LEAK" },
            },
          },
        }),
      );
    },
  });

  try {
    await page.goto(`${serve.baseUrl}/settings/mcp`);

    const panel = page.getByTestId("mcp-panel");
    await expect(panel).toBeVisible();

    // Both native servers render, tagged with their provenance.
    await expect(panel.getByText("fs", { exact: true })).toBeVisible();
    await expect(panel.getByText("remote", { exact: true })).toBeVisible();
    await expect(panel.getByText("agent-native:claude").first()).toBeVisible();

    // Secret values never reach the DOM; only the names do. The negative
    // assertions prove the env value (TOKEN) and header value (Authorization)
    // are redacted; the positive assertions prove their NAMES still render.
    await expect(panel).not.toContainText("SUPER_SECRET_DO_NOT_LEAK");
    await expect(panel).not.toContainText("HEADER_SECRET_DO_NOT_LEAK");
    await expect(panel).toContainText("TOKEN"); // env var name
    await expect(panel).toContainText("Authorization"); // header name
  } finally {
    await serve.stop();
  }
});
