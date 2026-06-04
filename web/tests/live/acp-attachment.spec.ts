// Structured view composer attachment round-trip (#1000 / #965).
//
// Boots `aoe serve` with the fake ACP agent advertising the `image`
// prompt capability (via a FAKE_ACP_SCRIPT), enables structured view, then
// drives the backend the composer drives: POST a prompt carrying an
// inline base64 image. Asserts the prompt persists with an attachment
// ref, the stored blob serves back over the replay GET endpoint with
// the right content type, and the capability gate / magic-byte sniff
// reject the cases they should.

import { test as base, expect } from "@playwright/test";
import { mkdtempSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import {
  spawnAoeServe,
  listSessions,
  seedSessionViaAoeAdd,
} from "../helpers/aoeServe";
import { enableStructuredViewAndWait } from "../helpers/acp";

// A valid 1x1 PNG. The leading bytes (89 50 4E 47) satisfy the server's
// image magic-byte sniff in validate_attachments.
const PNG_1X1_B64 =
  "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mNk+M9QDwADhgGAWjR9awAAAABJRU5ErkJggg==";

base("structured view prompt carries an image attachment end to end", async ({}, testInfo) => {
  const scriptDir = mkdtempSync(join(tmpdir(), "aoe-acp-attach-"));
  const scriptPath = join(scriptDir, "script.json");
  writeFileSync(
    scriptPath,
    JSON.stringify({ promptCapabilities: { image: true } }),
  );

  const serve = await spawnAoeServe({
    authMode: "none",
    acp: true,
    fakeAcpScript: scriptPath,
    workerIndex: testInfo.workerIndex,
    parallelIndex: testInfo.parallelIndex,
    seedFn: seedSessionViaAoeAdd({ title: "acp-attach" }),
  });

  try {
    const sessions = await listSessions(serve.baseUrl);
    const sessionId: string = sessions[0]!.id;
    await enableStructuredViewAndWait(serve.baseUrl, sessionId);

    const promptUrl = `${serve.baseUrl}/api/sessions/${sessionId}/acp/prompt`;

    // Happy path: text + one image attachment is accepted.
    const okRes = await fetch(promptUrl, {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify({
        text: "what is in this image?",
        attachments: [
          { kind: "image", mime_type: "image/png", data: PNG_1X1_B64, name: "shot.png" },
        ],
      }),
    });
    expect(okRes.status).toBeGreaterThanOrEqual(200);
    expect(okRes.status).toBeLessThan(300);

    // The persisted UserPromptSent carries a metadata-only ref; pull the
    // attachment id out of replay so we can fetch the stored blob.
    let attachmentId = "";
    await expect
      .poll(async () => {
        const replay = await fetch(
          `${serve.baseUrl}/api/sessions/${sessionId}/acp/replay?since=0`,
        ).then((r) => r.json());
        const frames: Array<{ event?: Record<string, unknown> }> = Array.isArray(
          replay,
        )
          ? replay
          : (replay?.frames ?? []);
        for (const f of frames) {
          const ups = f.event?.UserPromptSent as
            | { attachments?: Array<{ id: string; kind: string }> }
            | undefined;
          const att = ups?.attachments?.[0];
          if (att) {
            attachmentId = att.id;
            return att.kind;
          }
        }
        return null;
      })
      .toBe("image");

    // The stored blob serves back over the replay GET endpoint with the
    // right content type and bytes (PNG magic number preserved).
    const blobRes = await fetch(
      `${serve.baseUrl}/api/sessions/${sessionId}/acp/attachments/${attachmentId}`,
    );
    expect(blobRes.status).toBe(200);
    expect(blobRes.headers.get("content-type")).toContain("image/png");
    const bytes = new Uint8Array(await blobRes.arrayBuffer());
    expect(Array.from(bytes.slice(0, 4))).toEqual([0x89, 0x50, 0x4e, 0x47]);

    // Capability gate: the agent advertises image only, so an audio
    // attachment is rejected with 400.
    const audioRes = await fetch(promptUrl, {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify({
        text: "listen",
        attachments: [
          { kind: "audio", mime_type: "audio/mpeg", data: PNG_1X1_B64, name: "a.mp3" },
        ],
      }),
    });
    expect(audioRes.status).toBe(400);

    // Magic-byte sniff: text bytes mislabeled as image/png are rejected.
    const spoofRes = await fetch(promptUrl, {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify({
        text: "sneaky",
        attachments: [
          {
            kind: "image",
            mime_type: "image/png",
            data: Buffer.from("<svg>not an image</svg>").toString("base64"),
            name: "x.png",
          },
        ],
      }),
    });
    expect(spoofRes.status).toBe(400);
  } finally {
    await serve.stop();
  }
});
