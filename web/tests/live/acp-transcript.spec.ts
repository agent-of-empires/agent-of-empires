// Structured view transcript and composer against a real server: attachments, file links, cards, turn control.

import { spawnSync } from "node:child_process";
import { join } from "node:path";
import { crc32, deflateSync } from "node:zlib";
import { test, expect } from "../helpers/liveTest";
import { writeFileSync } from "node:fs";
import { fakeAcpScriptPath, resolveAoeBinary } from "../helpers/aoeServe";
import { commitAll, initWorkingRepo, writeFiles } from "../helpers/gitFixture";
import {
  HOLD,
  chunk,
  endTurn,
  enableStructuredViewAndWait,
  openSession,
  openStructuredView,
  postAcp,
  postPrompt,
  releaseTurn,
  replayFrames,
  replayJson,
  script,
  sessionIdByTitle,
  startAcpSession,
  stopButton,
  waitForStructuredView,
} from "../helpers/acp";

const MOD = process.platform === "darwin" ? "Meta" : "Control";
// A valid 1x1 PNG; its magic bytes satisfy the server's attachment sniff.
const PNG_1X1_B64 = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mNk+M9QDwADhgGAWjR9awAAAABJRU5ErkJggg==";
const IMAGE_CAPABLE = { promptCapabilities: { image: true } };

function aoeAdd(env: NodeJS.ProcessEnv, projectDir: string, title: string) {
  const res = spawnSync(resolveAoeBinary(), ["add", projectDir, "-t", title, "-c", "claude"], { env });
  if (res.status !== 0)
    throw new Error(`aoe add ${title} failed: status=${res.status} stderr=${res.stderr?.toString() ?? "<none>"}`);
}

const expect2xx = (res: Response) => {
  expect(res.status).toBeGreaterThanOrEqual(200);
  expect(res.status).toBeLessThan(300);
};

test("structured view prompt carries an image attachment end to end", async ({ spawnServe }) => {
  // #1000, #965
  const { serve, sessionId } = await startAcpSession(spawnServe, { title: "acp-attach", fakeAcpScript: IMAGE_CAPABLE });
  const prompt = (text: string, attachment: object) =>
    postAcp(serve.baseUrl, sessionId, "/prompt", { text, attachments: [attachment] });

  expect2xx(
    await prompt("what is in this image?", {
      kind: "image",
      mime_type: "image/png",
      data: PNG_1X1_B64,
      name: "shot.png",
    }),
  );

  // The persisted UserPromptSent carries a metadata-only ref to the stored blob.
  const attachment = async () =>
    (
      (await replayFrames(serve.baseUrl, sessionId)) as {
        event?: { UserPromptSent?: { attachments?: { id: string; kind: string }[] } };
      }[]
    )
      .map((f) => f.event?.UserPromptSent?.attachments?.[0])
      .find(Boolean);
  await expect.poll(async () => (await attachment())?.kind ?? null).toBe("image");
  const blobRes = await fetch(`${serve.baseUrl}/api/sessions/${sessionId}/acp/attachments/${(await attachment())!.id}`);
  expect(blobRes.status).toBe(200);
  expect(blobRes.headers.get("content-type")).toContain("image/png");
  expect(Array.from(new Uint8Array(await blobRes.arrayBuffer()).slice(0, 4))).toEqual([0x89, 0x50, 0x4e, 0x47]);

  // The agent advertises image only.
  expect(
    (await prompt("listen", { kind: "audio", mime_type: "audio/mpeg", data: PNG_1X1_B64, name: "a.mp3" })).status,
  ).toBe(400);
  // Text bytes labelled image/png fail the magic-byte sniff.
  const spoof = Buffer.from("<svg>not an image</svg>").toString("base64");
  expect((await prompt("sneaky", { kind: "image", mime_type: "image/png", data: spoof, name: "x.png" })).status).toBe(
    400,
  );
});

// A valid grayscale PNG with a real decodable size, so the browser computes a natural height.
function solidPng(width: number, height: number): Buffer {
  const chunk = (type: string, data: Buffer) => {
    const body = Buffer.concat([Buffer.from(type, "ascii"), data]);
    const out = Buffer.alloc(body.length + 8);
    out.writeUInt32BE(data.length, 0);
    body.copy(out, 4);
    out.writeUInt32BE(crc32(body), body.length + 4);
    return out;
  };
  const ihdr = Buffer.alloc(13);
  ihdr.writeUInt32BE(width, 0);
  ihdr.writeUInt32BE(height, 4);
  ihdr.writeUInt8(8, 8); // bit depth; colour type 0 (grayscale) stays zero
  const raw = Buffer.alloc((width + 1) * height, 0x80);
  for (let row = 0; row < height; row++) raw[row * (width + 1)] = 0; // filter: none
  return Buffer.concat([
    Buffer.from([0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a]),
    chunk("IHDR", ihdr),
    chunk("IDAT", deflateSync(raw)),
    chunk("IEND", Buffer.alloc(0)),
  ]);
}

test("uploaded image in a user message is capped in height and never overflows", async ({ page, spawnServe }) => {
  const { serve, sessionId } = await startAcpSession(spawnServe, {
    title: "acp-image-size",
    fakeAcpScript: IMAGE_CAPABLE,
  });
  expect2xx(
    await postAcp(serve.baseUrl, sessionId, "/prompt", {
      text: "tall picture",
      attachments: [
        { kind: "image", mime_type: "image/png", data: solidPng(100, 1200).toString("base64"), name: "tall.png" },
      ],
    }),
  );

  await openStructuredView(page, serve, sessionId);
  const image = page.locator("img.acp-artifact-image");
  await expect(image).toBeVisible({ timeout: 15_000 });
  await expect.poll(() => image.evaluate((el: HTMLImageElement) => el.naturalHeight)).toBe(1200);

  const box = (await image.boundingBox())!;
  expect(box.height).toBeLessThanOrEqual(320);
  // Aspect ratio survives the cap.
  const contentRatio = await image.evaluate((el: HTMLImageElement) => el.clientWidth / el.clientHeight);
  expect(contentRatio).toBeCloseTo(100 / 1200, 2);
  const viewport = page.viewportSize()!;
  expect(box.x + box.width).toBeLessThanOrEqual(viewport.width);
});

test("staged composer image survives session switch and reload", async ({ page, spawnServe }) => {
  // #2493: staged attachments persist like text drafts across the per-session remount.
  const serve = await spawnServe({
    acp: true,
    fakeAcpScript: IMAGE_CAPABLE,
    seedFn: ({ home, env }) => {
      aoeAdd(env, initWorkingRepo(join(home, "project-a"), env).path, "draft-source");
      aoeAdd(env, initWorkingRepo(join(home, "project-b"), env).path, "draft-target");
    },
  });
  const source = await sessionIdByTitle(serve.baseUrl, "draft-source");
  const target = await sessionIdByTitle(serve.baseUrl, "draft-target");
  await enableStructuredViewAndWait(serve.baseUrl, source, 30_000, serve.home);
  await enableStructuredViewAndWait(serve.baseUrl, target, 30_000, serve.home);

  await openStructuredView(page, serve, source);
  // Attach stays disabled until the agent's image capability arrives.
  await expect(page.getByRole("button", { name: /Attach files/ })).toBeEnabled({ timeout: 30_000 });
  await page.locator('input[type="file"]').setInputFiles({
    name: "shot.png",
    mimeType: "image/png",
    buffer: Buffer.from(PNG_1X1_B64, "base64"),
  });
  const chip = page.getByRole("img", { name: "shot.png" });
  await expect(chip).toBeVisible({ timeout: 10_000 });

  const palette = page.getByRole("dialog", { name: "Command palette" });
  const switchTo = async (title: string, id: string) => {
    await page.keyboard.press(`${MOD}+K`);
    await expect(palette).toBeVisible({ timeout: 5_000 });
    await palette.getByPlaceholder("Search actions, sessions, settings…").fill(title);
    await palette.getByText(title).first().click();
    await expect(page).toHaveURL(new RegExp(`/session/${id}`), { timeout: 10_000 });
    await waitForStructuredView(page);
  };
  await switchTo("draft-target", target);
  await expect(chip).toHaveCount(0);
  await switchTo("draft-source", source);
  await expect(chip).toBeVisible({ timeout: 10_000 });
  await page.reload();
  await waitForStructuredView(page);
  await expect(chip).toBeVisible({ timeout: 10_000 });
});

test("structured view transcript file links open in-app, scroll to cited line, and render out-of-repo paths as inert text", async ({
  page,
  spawnServe,
}) => {
  // #1718, #1809, #1810, #2587. Line 80 of a fully rewritten file is virtualized below the fold.
  const sentinel = "TARGET_SENTINEL_424242";
  const lines = Array.from({ length: 100 }, (_, i) => `export const v${i} = ${i};`);
  const serve = await spawnServe({
    acp: true,
    // Rewritten in seedFn once the project path is known.
    fakeAcpScript: script(),
    seedFn: ({ home, env }) => {
      const projectDir = initWorkingRepo(join(home, "project"), env).path;
      writeFiles(projectDir, {
        "src/a.ts": "export const a = 1;\n",
        "src/b.ts": "export const unchangedConst = 42;\n",
        "src/long.ts": lines.join("\n") + "\n",
      });
      commitAll(projectDir, "baseline", env);
      writeFiles(projectDir, {
        "src/a.ts": "export const a = 11;\n",
        "src/long.ts":
          lines.map((l, i) => (i === 79 ? `export const ${sentinel} = ${i};` : `${l} // edited`)).join("\n") + "\n",
      });
      // project_path is the working tree, so absolute links under it resolve to repo files.
      const text = `See [a.ts](${projectDir}/src/a.ts:1), [b.ts](${projectDir}/src/b.ts:1), [deep](${projectDir}/src/long.ts:80) and [missing](/tmp/aoe-1718-not-a-repo/missing.ts:1).`;
      writeFileSync(fakeAcpScriptPath(home), JSON.stringify(script(endTurn(chunk(text)))));
      aoeAdd(env, projectDir, "acp-filelink");
    },
  });
  const sessionId = await sessionIdByTitle(serve.baseUrl, "acp-filelink");
  await enableStructuredViewAndWait(serve.baseUrl, sessionId);
  await openStructuredView(page, serve, sessionId);
  expect2xx(await postPrompt(serve.baseUrl, sessionId, "show me the file"));

  const sessionUrl = new RegExp(`/session/${sessionId}`);
  await expect(page.locator("span.acp-inert-path", { hasText: "missing" })).toBeVisible({ timeout: 15_000 });
  await expect(page.getByRole("link", { name: "missing" })).toHaveCount(0);
  await expect(page).toHaveURL(sessionUrl);

  const links: [string, RegExp][] = [
    ["a.ts", /export const a = 11/],
    // Only visible if the viewer scrolled to the cited line.
    ["deep", new RegExp(sentinel)],
    // No diff against base, so the full-file fallback renders.
    ["b.ts", /export const unchangedConst = 42/],
  ];
  for (const [name, content] of links) {
    if (name !== "a.ts") await page.getByRole("button", { name: "Back to terminal" }).click();
    const link = page.getByRole("link", { name });
    await expect(link).toBeVisible();
    await link.click();
    await expect(page.getByText(content).first()).toBeVisible({ timeout: 10_000 });
    await expect(page).toHaveURL(sessionUrl);
  }
});

test("structured view renders synthesize memory recall as cleaned markdown", async ({ page, spawnServe }) => {
  // #2142: the SDK's recall payload is a <system-reminder> envelope around cat -n numbered markdown.
  const dirtyText =
    "<system-reminder>\n     1\t# User profile\n     2\t\n     3\tUser is a senior engineer working on agent-of-empires.\n" +
    "     4\t\n     5\t- prefers terse output\n     6\t- no em dashes\n</system-reminder>";
  // `-c claude` selects the agent profile that supports the memory recall tool.
  const { serve, sessionId } = await startAcpSession(spawnServe, {
    title: "acp-memrecall",
    fakeAcpScript: script(
      endTurn({
        sessionUpdate: "tool_call",
        toolCallId: "mem-synth-1",
        title: "Recalled synthesized memory",
        kind: "read",
        status: "completed",
        content: [{ type: "content", content: { type: "text", text: dirtyText } }],
        _meta: { claudeCode: { toolName: "memory_recall", toolResponse: { mode: "synthesize" } } },
      }),
    ),
  });
  await openStructuredView(page, serve, sessionId);
  expect2xx(await postPrompt(serve.baseUrl, sessionId, "what do you remember"));

  const cardToggle = page.getByRole("button").filter({ hasText: "Synthesised memory" });
  await expect(cardToggle).toBeVisible({ timeout: 15_000 });
  await cardToggle.click();
  const body = page.getByTestId("memory-recall-synthesized");
  await expect(body).toBeVisible();
  await expect(body).toContainText("User is a senior engineer working on agent-of-empires.");
  await expect(body).toContainText("prefers terse output");
  await expect(body).not.toContainText("system-reminder");
  expect(await body.innerText()).not.toMatch(/\d+\t/);
  await expect(body.locator("h1")).toHaveText("User profile");
  await expect(body.locator("li")).toHaveCount(2);
});

test("Escape inside the structured view composer does not POST /acp/cancel", async ({ page, spawnServe }) => {
  const { serve, sessionId } = await startAcpSession(spawnServe, {
    title: "escape-no-cancel",
    fakeAcpScript: script(endTurn(chunk("ESCAPE_TURN_ACTIVE"), HOLD, chunk("ESCAPE_TURN_COMPLETED"))),
  });
  let cancelCount = 0;
  page.on("request", (req) => {
    if (req.method() === "POST" && req.url().endsWith(`/api/sessions/${sessionId}/acp/cancel`)) cancelCount++;
  });
  await openSession(page, serve, sessionId);
  await waitForStructuredView(page);
  const composer = page.locator('textarea[name="input"]');
  await composer.fill("stay in the turn");
  await composer.press("Enter");
  await expect(page.getByText("ESCAPE_TURN_ACTIVE", { exact: true })).toBeVisible();
  await expect(stopButton(page)).toBeVisible();

  await composer.press("Escape");
  releaseTurn(serve);
  await expect(page.getByText(/ESCAPE_TURN_COMPLETED/)).toBeVisible();
  await expect(page.getByRole("textbox", { name: /Send a message/i })).toBeVisible();
  expect(cancelCount).toBe(0);
});

test("the spinner names the compaction phase and hides the force-end hatch", async ({ page, spawnServe }) => {
  // #3219: /compact is silent for minutes; it must not look wedged, offer a hatch that aborts it, or steer a follow-up.
  const { serve, sessionId } = await startAcpSession(spawnServe, {
    title: "acp-compaction-ui",
    fakeAcpScript: script(
      endTurn(chunk("Compacting..."), HOLD, chunk("\n\nCompacting completed.")),
      endTurn(chunk("answered after compaction")),
    ),
    extraEnv: { FAKE_ACP_STEERING: "1" },
  });
  await openStructuredView(page, serve, sessionId);
  // By form name: the accessible name changes while a turn runs.
  const composer = page.locator('textarea[name="input"]');
  // Started over REST to avoid the slash picker; the follow-up below uses the real composer.
  await postPrompt(serve.baseUrl, sessionId, "/compact");

  const spinner = page.getByTestId("acp-working-spinner");
  await expect(spinner).toContainText(/Compaction in progress/i, { timeout: 10_000 });
  await composer.fill("also check the tests");
  await composer.press("Enter");
  await expect(page.getByRole("button", { name: /^also check the tests$/ })).toBeVisible({ timeout: 5_000 });
  await expect(spinner).toContainText(/Compaction in progress/i);
  await expect(page.getByText("answered after compaction")).toHaveCount(0);
  await expect(spinner).not.toContainText(/Waiting on model/i);
  await expect(page.getByRole("button", { name: /force end turn/i })).toHaveCount(0);

  releaseTurn(serve);
  await expect(page.getByText("Compacting completed.")).toBeVisible({ timeout: 15_000 });
  await expect(page.getByText("answered after compaction")).toBeVisible({ timeout: 15_000 });
  const json = await replayJson(serve.baseUrl, sessionId);
  expect(json).toContain("ConversationCompactionStarted");
  expect(json).not.toContain("steered: also check the tests");
});
