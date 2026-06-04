// Structured view replay-catchup.
//
// `GET /api/sessions/:id/acp/replay?since=N` returns
// `{ frames, lost, highest_seq, lowest_seq }` out of the disk-backed
// event store. This spec seeds a deterministic event stream via
// `structured view/force_end_turn` (each call publishes a `Stopped` event with a
// fresh seq), then verifies:
//   - replay from since=0 returns every seeded frame
//   - replay from since=highest returns no frames but reports highest_seq
//   - replay from a since cursor predating the lowest stored seq sets
//     `lost: true` (simulated via a synthetic far-past since on a fresh
//     session; lowest_seq starts at 1 so since=0 alone is not enough)
//
// Independent of #1237: force_end_turn writes straight to the event
// store without going through the prompt path.

import { test, expect } from "@playwright/test";
import {
  spawnAoeServe,
  listSessions,
  seedSessionViaAoeAdd,
} from "../helpers/aoeServe";

const SEED_EVENTS = 5;

test("structured view/replay surfaces seeded events and signals lost frames", async ({}, testInfo) => {
  const serve = await spawnAoeServe({
    authMode: "none",
    acp: true,
    workerIndex: testInfo.workerIndex,
    parallelIndex: testInfo.parallelIndex,
    seedFn: seedSessionViaAoeAdd({ title: "acp-replay" }),
  });

  try {
    const sessions = await listSessions(serve.baseUrl);
    const sessionId = sessions[0]!.id;

    await fetch(
      `${serve.baseUrl}/api/sessions/${sessionId}/acp/enable`,
      { method: "POST" },
    );

    for (let i = 0; i < SEED_EVENTS; i++) {
      const r = await fetch(
        `${serve.baseUrl}/api/sessions/${sessionId}/acp/force_end_turn`,
        { method: "POST" },
      );
      expect(r.status).toBe(202);
    }

    // Poll for at least SEED_EVENTS user_forced events to land in the
    // store. Other events (ModesAvailable, AvailableCommandsUpdated)
    // may interleave from the supervisor startup; we just care that the
    // user_forced count reaches the seeded value.
    let body: {
      frames: { seq: number }[];
      lost: boolean;
      highest_seq: number | null;
      lowest_seq: number | null;
    } | null = null;
    await expect
      .poll(
        async () => {
          const res = await fetch(
            `${serve.baseUrl}/api/sessions/${sessionId}/acp/replay?since=0`,
          );
          if (!res.ok) return -1;
          body = await res.json();
          return (
            JSON.stringify(body!.frames).split('"user_forced"').length - 1
          );
        },
        { timeout: 15_000, intervals: [100, 200, 500, 1000] },
      )
      .toBeGreaterThanOrEqual(SEED_EVENTS);
    expect(body).not.toBeNull();
    expect(body!.frames.length).toBeGreaterThanOrEqual(SEED_EVENTS);
    expect(body!.lowest_seq).not.toBeNull();
    expect(body!.highest_seq).not.toBeNull();
    expect(body!.lost).toBe(false);
    // Seq is monotonic in the response.
    for (let i = 1; i < body!.frames.length; i++) {
      expect(body!.frames[i]!.seq).toBeGreaterThan(body!.frames[i - 1]!.seq);
    }

    // since=highest_seq returns an empty frames array, still reports the
    // current head.
    const highest = body!.highest_seq!;
    const tail = await fetch(
      `${serve.baseUrl}/api/sessions/${sessionId}/acp/replay?since=${highest}`,
    ).then((r) => r.json());
    expect(tail.frames.length).toBe(0);
    expect(tail.highest_seq).toBe(highest);
    expect(tail.lost).toBe(false);

    // Pagination: with a small `limit`, the endpoint returns bounded
    // pages plus `next_cursor`/`has_more`, and following the cursor to
    // exhaustion reassembles the same transcript a single unbounded
    // (default-page) replay returns. `target` caps the loop at the
    // snapshot head so a stray startup event appended mid-loop can't
    // make the two reads disagree.
    const full = body!;
    const target = full.highest_seq!;
    const fullSeqs = full.frames
      .map((f) => f.seq)
      .filter((s) => s <= target);

    const PAGE = 2;
    const pagedSeqs: number[] = [];
    let cursor = 0;
    let pages = 0;
    for (;;) {
      const page = (await fetch(
        `${serve.baseUrl}/api/sessions/${sessionId}/acp/replay?since=${cursor}&limit=${PAGE}`,
      ).then((r) => r.json())) as {
        frames: { seq: number }[];
        lost: boolean;
        highest_seq: number;
        next_cursor: number | null;
        has_more: boolean;
      };
      pages++;
      expect(page.frames.length).toBeLessThanOrEqual(PAGE);
      for (const f of page.frames) {
        if (f.seq <= target) pagedSeqs.push(f.seq);
      }
      const next = page.next_cursor;
      if (page.has_more && next != null && next > cursor && next < target) {
        cursor = next;
        continue;
      }
      break;
    }

    expect(pages).toBeGreaterThan(1);
    expect(pagedSeqs).toEqual(fullSeqs);
  } finally {
    await serve.stop();
  }
});
