import { test } from "node:test";
import assert from "node:assert/strict";
import { mkdtemp, writeFile, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import {
  appendTurn,
  createTranscript,
  loadTranscript,
} from "../src/transcript.ts";

const ID = "a".repeat(32);
const FILE = `aoe-agent-${ID}.jsonl`;

/** Run `body` against a fresh directory, removed however it ends. */
async function inTmpDir(body: (dir: string) => Promise<void>): Promise<void> {
  const dir = await mkdtemp(join(tmpdir(), "aoe-agent-transcript-"));
  try {
    await body(dir);
  } finally {
    await rm(dir, { recursive: true, force: true });
  }
}

/** Write a transcript file directly, bypassing `appendTurn`. */
async function writeRecords(dir: string, records: unknown[]): Promise<void> {
  const lines = records.map((r) =>
    typeof r === "string" ? r : JSON.stringify(r),
  );
  await writeFile(join(dir, FILE), lines.join("\n") + "\n");
}

test("round-trips appended exchanges in order, newlines included", () =>
  inTmpDir(async (dir) => {
    await createTranscript(dir, ID);
    await appendTurn(dir, ID, "hello", "hi there");
    await appendTurn(dir, ID, "line1\nline2", "reply\nwith\nnewlines");
    assert.deepEqual(await loadTranscript(dir, ID), [
      { role: "user", content: "hello" },
      { role: "assistant", content: "hi there" },
      { role: "user", content: "line1\nline2" },
      { role: "assistant", content: "reply\nwith\nnewlines" },
    ]);
  }));

test("missing native file fails rather than claiming an empty resume", () =>
  inTmpDir(async (dir) => {
    await assert.rejects(loadTranscript(dir, ID), { code: "ENOENT" });
  }));

test("skips malformed and schema-invalid records", () =>
  inTmpDir(async (dir) => {
    await writeRecords(dir, [
      { role: "user", content: "keep me" },
      "{ not valid json",
      { role: "system", content: "wrong role" },
      { role: "assistant", content: 42 },
      { role: "assistant", content: "keep me too" },
    ]);
    assert.deepEqual(await loadTranscript(dir, ID), [
      { role: "user", content: "keep me" },
      { role: "assistant", content: "keep me too" },
    ]);
  }));

test("drops a trailing lone user record (torn write)", () =>
  inTmpDir(async (dir) => {
    await writeRecords(dir, [
      { role: "user", content: "q1" },
      { role: "assistant", content: "a1" },
      { role: "user", content: "q2 with no reply" },
    ]);
    assert.deepEqual(await loadTranscript(dir, ID), [
      { role: "user", content: "q1" },
      { role: "assistant", content: "a1" },
    ]);
  }));

test("creating an existing native ID cannot erase its history", () =>
  inTmpDir(async (dir) => {
    await assert.rejects(appendTurn(dir, ID, "unregistered", "reply"), {
      code: "ENOENT",
    });
    await createTranscript(dir, ID);
    await appendTurn(dir, ID, "keep", "reply");
    await assert.rejects(createTranscript(dir, ID), { code: "EEXIST" });
    assert.deepEqual(await loadTranscript(dir, ID), [
      { role: "user", content: "keep" },
      { role: "assistant", content: "reply" },
    ]);
  }));
