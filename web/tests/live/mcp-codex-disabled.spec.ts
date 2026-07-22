// Live: native Codex MCP entries with `enabled = false` stay out of the
// effective set rendered by the dashboard's MCP settings panel.

import { test as base, expect } from "@playwright/test";
import { mkdirSync, writeFileSync } from "node:fs";
import { join } from "node:path";
import { appDirFor, resolveAoeBinary, spawnAoeServe } from "../helpers/aoeServe";

base("MCP panel excludes disabled native Codex servers", async ({ page }, testInfo) => {
  const serve = await spawnAoeServe({
    authMode: "none",
    workerIndex: testInfo.workerIndex,
    parallelIndex: testInfo.parallelIndex,
    seedFn: ({ home, xdg }) => {
      const codexDir = join(home, ".codex");
      mkdirSync(codexDir, { recursive: true });
      writeFileSync(
        join(codexDir, "config.toml"),
        `
[mcp_servers.omitted]
command = "mcp-omitted"

[mcp_servers.explicit_true]
command = "mcp-true"
enabled = true

[mcp_servers.explicit_false]
command = "mcp-false"
enabled = false
`,
      );

      const appDir = appDirFor(home, xdg, resolveAoeBinary());
      mkdirSync(appDir, { recursive: true });
      writeFileSync(
        join(appDir, "config.toml"),
        `
[session]
default_tool = "codex"
`,
      );
    },
  });

  try {
    await page.goto(`${serve.baseUrl}/settings/mcp`);

    const panel = page.getByTestId("mcp-panel");
    await expect(panel).toBeVisible();
    await expect(panel).toContainText("Effective set forwarded to codex");
    await expect(panel.getByText("omitted", { exact: true })).toBeVisible();
    await expect(panel.getByText("explicit_true", { exact: true })).toBeVisible();
    await expect(panel.getByText("explicit_false", { exact: true })).toHaveCount(0);
    await expect(panel.getByText("agent-native:codex").first()).toBeVisible();
  } finally {
    await serve.stop();
  }
});
