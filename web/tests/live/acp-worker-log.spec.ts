// Live coverage for GET /api/sessions/:id/acp/worker-log.
//
// Endpoint surfaces the per-session runner stderr drain (same content
// `aoe acp logs --session <id>` reads) so a dashboard user without
// host terminal access can see verbatim adapter errors. See #1449.

import { test as base, expect } from "@playwright/test";
import {
  spawnAoeServe,
  listSessions,
  seedSessionViaAoeAdd,
} from "../helpers/aoeServe";
import { enableStructuredViewAndWait } from "../helpers/acp";

interface WorkerLogResponse {
  path: string;
  exists: boolean;
  tail: string;
  lines_returned: number;
  truncated: boolean;
}

base(
  "worker-log returns exists=false before the runner writes anything",
  async ({}, testInfo) => {
    const title = "worker-log-empty";
    const serve = await spawnAoeServe({
      authMode: "none",
      acp: true,
      workerIndex: testInfo.workerIndex,
      parallelIndex: testInfo.parallelIndex,
      seedFn: seedSessionViaAoeAdd({ title }),
    });

    try {
      const sessions = await listSessions(serve.baseUrl);
      const seeded = sessions.find((s) => s.title === title);
      if (!seeded) throw new Error(`seeded session '${title}' missing`);
      const sessionId = seeded.id;

      const res = await fetch(
        `${serve.baseUrl}/api/sessions/${sessionId}/acp/worker-log?tail=200`,
      );
      expect(res.status).toBe(200);
      const body = (await res.json()) as WorkerLogResponse;
      expect(body.exists).toBe(false);
      expect(body.tail).toBe("");
      expect(body.lines_returned).toBe(0);
      expect(body.truncated).toBe(false);
      expect(body.path).toMatch(/acp-workers/);
    } finally {
      await serve.stop();
    }
  },
);

base(
  "worker-log returns the runner tail after structured view spawns",
  async ({}, testInfo) => {
    const title = "worker-log-populated";
    const serve = await spawnAoeServe({
      authMode: "none",
      acp: true,
      workerIndex: testInfo.workerIndex,
      parallelIndex: testInfo.parallelIndex,
      seedFn: seedSessionViaAoeAdd({ title }),
    });

    try {
      const sessions = await listSessions(serve.baseUrl);
      const seeded = sessions.find((s) => s.title === title);
      if (!seeded) throw new Error(`seeded session '${title}' missing`);
      const sessionId = seeded.id;

      await enableStructuredViewAndWait(serve.baseUrl, sessionId);

      // The runner emits a handful of `tracing` lines during init
      // (`acp.runner`, `acp.acp.spawn`, etc.) before quiescing.
      // Poll briefly until the tail is non-empty so the assertion does
      // not race the post-handshake log flush.
      const deadline = Date.now() + 10_000;
      let body: WorkerLogResponse | null = null;
      while (Date.now() < deadline) {
        const res = await fetch(
          `${serve.baseUrl}/api/sessions/${sessionId}/acp/worker-log?tail=200`,
        );
        expect(res.status).toBe(200);
        body = (await res.json()) as WorkerLogResponse;
        if (body.exists && body.lines_returned > 0) break;
        await new Promise((r) => setTimeout(r, 200));
      }
      expect(body, "worker-log never reported a populated tail").not.toBeNull();
      expect(body!.exists).toBe(true);
      expect(body!.lines_returned).toBeGreaterThan(0);
      expect(body!.tail.length).toBeGreaterThan(0);
    } finally {
      await serve.stop();
    }
  },
);

base(
  "worker-log clamps an oversized tail request to the server max",
  async ({}, testInfo) => {
    const title = "worker-log-clamp";
    const serve = await spawnAoeServe({
      authMode: "none",
      acp: true,
      workerIndex: testInfo.workerIndex,
      parallelIndex: testInfo.parallelIndex,
      seedFn: seedSessionViaAoeAdd({ title }),
    });

    try {
      const sessions = await listSessions(serve.baseUrl);
      const seeded = sessions.find((s) => s.title === title);
      if (!seeded) throw new Error(`seeded session '${title}' missing`);
      const sessionId = seeded.id;
      await enableStructuredViewAndWait(serve.baseUrl, sessionId);

      const res = await fetch(
        `${serve.baseUrl}/api/sessions/${sessionId}/acp/worker-log?tail=999999`,
      );
      expect(res.status).toBe(200);
      const body = (await res.json()) as WorkerLogResponse;
      // Hard ceiling enforced server-side. The fact the request did not
      // 4xx is the contract: clamping is silent.
      expect(body.lines_returned).toBeLessThanOrEqual(2000);
    } finally {
      await serve.stop();
    }
  },
);

base(
  "worker-log returns 404 for an unknown session id",
  async ({}, testInfo) => {
    const serve = await spawnAoeServe({
      authMode: "none",
      acp: true,
      workerIndex: testInfo.workerIndex,
      parallelIndex: testInfo.parallelIndex,
      seedFn: seedSessionViaAoeAdd({ title: "worker-log-404" }),
    });

    try {
      const res = await fetch(
        `${serve.baseUrl}/api/sessions/does-not-exist-9d34/acp/worker-log`,
      );
      expect(res.status).toBe(404);
    } finally {
      await serve.stop();
    }
  },
);
