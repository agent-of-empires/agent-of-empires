// Suggested sessions against the real server: a worker publishes, and preferences, templates, mappings and
// dismissals persist across reads. Read-only mode refuses writes and offers no Start or Dismiss.

import { spawnSync } from "node:child_process";
import { mkdirSync, writeFileSync } from "node:fs";
import { join } from "node:path";
import { test, expect, type ServeHandle } from "../helpers/liveTest";
import { resolveAoeBinary } from "../helpers/aoeServe";

const MANIFEST = `
id = "acme.sugg"
name = "Acme"
version = "0.1.0"
api_version = 21
capabilities = ["runtime.worker"]

[suggestions]
[[suggestions.sources]]
criterion = "assigned"
label = "Assigned issues"
description = "Issues assigned to you."
variables = ["n"]

[runtime]
kind = "command"
system = true
command = ["node", "worker.js"]
`;

const WORKER = `
let id = 0;
const send = (method, params) => process.stdout.write(JSON.stringify({ jsonrpc: "2.0", id: ++id, method, params }) + "\\n");
const item = (n) => ({
  id: "issue-" + n,
  title: "Fix issue " + n,
  reason: "Assigned to you",
  source_url: "https://example.com/issues/" + n,
  source_ref: "acme/web#" + n,
  preview: "",
  mapping_key: "acme/web",
  criterion: "assigned",
  variables: { n: String(n) },
  draft: { title: "Fix issue " + n, prompt: "Look at issue " + n },
});
setTimeout(() => send("suggestions.set", { items: [item(1), item(2)] }), 200);
setInterval(() => {}, 1000);
`;

const seedProvider = ({ home, env }: { home: string; env: NodeJS.ProcessEnv }) => {
  const src = join(home, "acme-sugg-src");
  mkdirSync(src, { recursive: true });
  writeFileSync(join(src, "aoe-plugin.toml"), MANIFEST);
  writeFileSync(join(src, "worker.js"), WORKER);
  const res = spawnSync(resolveAoeBinary(), ["plugin", "install", src, "--yes"], { env });
  if (res.status !== 0) throw new Error(`plugin install failed: ${res.stderr?.toString()}`);
  mkdirSync(join(home, "proj"), { recursive: true });
  const added = spawnSync(resolveAoeBinary(), ["project", "add", join(home, "proj"), "--name", "proj"], { env });
  if (added.status !== 0) throw new Error(`project add failed: ${added.stderr?.toString()}`);
};

const api = (serve: ServeHandle, path: string, init?: RequestInit) =>
  fetch(`${serve.baseUrl}/api/suggestions${path}`, {
    ...init,
    headers: { "Content-Type": "application/json", ...(init?.headers ?? {}) },
  });
const snapshot = async (serve: ServeHandle) => (await api(serve, "")).json();
const waitForItems = (serve: ServeHandle) =>
  expect.poll(async () => (await snapshot(serve)).items?.length ?? 0, { timeout: 30_000 }).toBe(2);

test("preferences, templates, mappings and dismissals round-trip through the server", async ({ spawnServe, page }) => {
  const serve = await spawnServe({ seedFn: seedProvider });
  await waitForItems(serve);

  const prefs = await api(serve, "/prefs", { method: "PATCH", body: JSON.stringify({ sidebar: true }) });
  expect(prefs.status).toBe(200);
  expect((await snapshot(serve)).prefs.sidebar).toBe(true);

  const put = await api(serve, "/templates", {
    method: "PUT",
    body: JSON.stringify({ provider_id: "acme.sugg", criterion: "assigned", template: "Work on {n}" }),
  });
  expect(put.status).toBe(200);
  const stored = await (await api(serve, "/templates?provider_id=acme.sugg&criterion=assigned")).json();
  expect(stored.template).toBe("Work on {n}");

  const mapped = await api(serve, "/mappings", {
    method: "PUT",
    body: JSON.stringify({ provider_id: "acme.sugg", mapping_key: "acme/web", projects: [join(serve.home, "proj")] }),
  });
  expect(mapped.status, await mapped.clone().text()).toBe(200);
  const mappings = await (await api(serve, "/mappings")).json();
  expect(mappings.mappings).toHaveLength(1);
  const removed = await api(
    serve,
    `/mappings?${new URLSearchParams({ provider_id: "acme.sugg", mapping_key: "acme/web" })}`,
    {
      method: "DELETE",
    },
  );
  expect(removed.status).toBe(200);
  expect((await (await api(serve, "/mappings")).json()).mappings).toHaveLength(0);

  await page.goto(`${serve.baseUrl}/suggestions`);
  const row = page.getByText("Fix issue 1").locator("xpath=ancestor::li[1]");
  await expect(row).toBeVisible();
  await row.getByRole("button", { name: "Dismiss" }).click();
  await expect
    .poll(async () => (await snapshot(serve)).items.find((i: { id: string }) => i.id === "issue-1")?.dismissed)
    .toBe(true);
});

// A read-only server does not run workers, so there is nothing to publish; the Start and Dismiss UI is covered by the mocked specs.
test("read-only refuses every suggestions write", async ({ spawnServe }) => {
  const serve = await spawnServe({ seedFn: seedProvider, readOnly: true });
  const body = (b: object) => JSON.stringify(b);
  const writes: [string, string, object][] = [
    ["PATCH", "/prefs", { sidebar: true }],
    ["PATCH", "/sources", { provider_id: "acme.sugg", criterion: "assigned", enabled: false }],
    ["PUT", "/templates", { provider_id: "acme.sugg", criterion: "assigned", template: "x" }],
    ["PUT", "/mappings", { provider_id: "acme.sugg", mapping_key: "acme/web", projects: [] }],
    ["POST", "/dismiss", { provider_id: "acme.sugg", id: "issue-1" }],
    ["POST", "/refresh", {}],
  ];
  for (const [method, path, payload] of writes) {
    const res = await api(serve, path, { method, body: body(payload) });
    expect(res.status, `${method} ${path}`).toBe(403);
  }
  expect((await snapshot(serve)).prefs.sidebar).toBe(false);
});
