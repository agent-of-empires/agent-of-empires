// Settings persistence round-trip for the cockpit panel.
//
// Sibling of settings-persistence-tmux.spec.ts. Guards #1689: the cockpit
// section was missing from the web settings allowlist, so every cockpit
// field (including the new idle auto-stop) silently failed to save with
// validation_failed. The mocked folds Vitest masked it because it stubs
// the API. This drives the REAL server: a cockpit PATCH must persist a
// safe knob (auto_stop_idle_secs) and must strip the node_path binary
// override (COCKPIT_BLOCKED_FIELDS, an RCE surface that stays local-only).

import { test, expect } from "../helpers/liveTest";

test("cockpit settings persist through PATCH + reload, node_path is stripped", async ({
  serve,
  page,
}) => {
  const before = await fetch(`${serve.baseUrl}/api/settings`).then((r) =>
    r.json(),
  );
  const baselineCockpit = (before?.cockpit ?? {}) as Record<string, unknown>;
  const baselineNodePath =
    typeof baselineCockpit.node_path === "string"
      ? (baselineCockpit.node_path as string)
      : "";
  const newIdle =
    baselineCockpit.auto_stop_idle_secs === 28800 ? 14400 : 28800;

  // PATCH a safe knob plus a malicious node_path through the same endpoint
  // the dashboard hits. The section must be accepted (regression) and
  // node_path must be ignored (security).
  const patchRes = await fetch(`${serve.baseUrl}/api/settings`, {
    method: "PATCH",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify({
      cockpit: {
        ...baselineCockpit,
        auto_stop_idle_secs: newIdle,
        node_path: "/tmp/evil-node",
      },
    }),
  });
  expect(patchRes.ok).toBeTruthy();

  // Server-side: the safe knob persisted, node_path was stripped (still
  // the baseline, never the injected /tmp/evil-node).
  const after = await fetch(`${serve.baseUrl}/api/settings`).then((r) =>
    r.json(),
  );
  expect(after?.cockpit?.auto_stop_idle_secs).toBe(newIdle);
  expect(after?.cockpit?.node_path).toBe(baselineNodePath);
  expect(after?.cockpit?.node_path).not.toBe("/tmp/evil-node");

  // Frontend-side: reload and the persisted value is what the page reads.
  await page.goto(serve.baseUrl);
  const fetched = await page.evaluate(async (url) => {
    const r = await fetch(`${url}/api/settings`);
    return r.json();
  }, serve.baseUrl);
  expect(fetched?.cockpit?.auto_stop_idle_secs).toBe(newIdle);
  expect(fetched?.cockpit?.node_path).not.toBe("/tmp/evil-node");
});
