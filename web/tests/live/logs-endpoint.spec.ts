// GET /api/system/logs exposes host logs, so its access boundary runs through a real server:
// redacted content for an authenticated client, refusal otherwise. The redaction rules and tail
// bounds are unit-tested in src/server/api/logs.rs.

import { writeFileSync } from "node:fs";
import { join } from "node:path";
import { test, expect, type ServeOptions } from "../helpers/liveTest";
import { appDirFor, resolveAoeBinary } from "../helpers/aoeServe";

const MARKER = "logs-endpoint-marker-line";
const SECRETS = ["tok-live-secret", "eyJ-live-bearer", "sid-live-cookie", "hunter2-live"];
const FIXTURE = [
  MARKER,
  `GET /?token=${SECRETS[0]} HTTP/1.1`,
  `Authorization: Bearer ${SECRETS[1]}`,
  `Cookie: sid=${SECRETS[2]}`,
  `password=${SECRETS[3]}`,
  "",
].join("\n");

// The server appends to this file after boot, so the fixture stays within the default tail.
const seedFn: NonNullable<ServeOptions["seedFn"]> = ({ home, xdg }) => {
  writeFileSync(join(appDirFor(home, xdg, resolveAoeBinary()), "debug.log"), FIXTURE);
};

const LOGS = "/api/system/logs?lines=2000";

function expectNoLeak(body: string) {
  expect(body).not.toContain(MARKER);
  for (const secret of SECRETS) expect(body).not.toContain(secret);
}

// Token mode: loopback callers skip the passphrase wall, but never the token.
test("a valid token gets the redacted tail; missing or wrong tokens are refused", async ({ spawnServe }) => {
  const handle = await spawnServe({ authMode: "token", seedFn });
  const bearer = { Authorization: `Bearer ${handle.authToken}` };

  for (const headers of [{}, { Authorization: `Bearer ${"z".repeat(64)}` }]) {
    const refused = await fetch(`${handle.baseUrl}${LOGS}`, { headers });
    expect(refused.status).toBe(401);
    expectNoLeak(await refused.text());
  }

  const res = await fetch(`${handle.baseUrl}${LOGS}`, { headers: bearer });
  expect(res.status).toBe(200);
  expect(res.headers.get("cache-control")).toBe("no-store");
  const body = await res.json();
  expect(body.status).toBe("available");
  expect(body.content).toContain(MARKER);
  expect(body.content).toContain("[REDACTED]");
  for (const secret of SECRETS) expect(body.content).not.toContain(secret);

  const bad = await fetch(`${handle.baseUrl}/api/system/logs?lines=0`, { headers: bearer });
  expect(bad.status).toBe(400);
});

test("read-only servers refuse the endpoint", async ({ spawnServe }) => {
  const handle = await spawnServe({ readOnly: true, seedFn });
  const res = await fetch(`${handle.baseUrl}${LOGS}`);
  expect(res.status).toBe(403);
  const text = await res.text();
  expect(JSON.parse(text)).toMatchObject({ error: "read_only" });
  expectNoLeak(text);
});

test("CityHall client mode refuses the endpoint", async ({ spawnServe }) => {
  const handle = await spawnServe({ extraArgs: ["--cityhall"], seedFn });
  const res = await fetch(`${handle.baseUrl}${LOGS}`);
  expect(res.status).toBe(403);
  const text = await res.text();
  expect(JSON.parse(text)).toMatchObject({ error: "cityhall_mode" });
  expectNoLeak(text);
});
