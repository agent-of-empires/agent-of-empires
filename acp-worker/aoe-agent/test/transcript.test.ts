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

async function tmpDir(): Promise<string> {
  return mkdtemp(join(tmpdir(), "aoe-agent-transcript-"));
}

test("round-trips appended exchanges in order", async () => {
  const dir = await tmpDir();
  try {
    await createTranscript(dir, ID);
    await appendTurn(dir, ID, "hello", "hi there");
    await appendTurn(dir, ID, "again", "yep");
    const messages = await loadTranscript(dir, ID);
    assert.deepEqual(messages, [
      { role: "user", content: "hello" },
      { role: "assistant", content: "hi there" },
      { role: "user", content: "again" },
      { role: "assistant", content: "yep" },
    ]);
  } finally {
    await rm(dir, { recursive: true, force: true });
  }
});

test("missing native file fails rather than claiming an empty resume", async () => {
  const dir = await tmpDir();
  try {
    await assert.rejects(loadTranscript(dir, ID), { code: "ENOENT" });
  } finally {
    await rm(dir, { recursive: true, force: true });
  }
});

test("skips malformed and schema-invalid records", async () => {
  const dir = await tmpDir();
  try {
    const lines = [
      JSON.stringify({ role: "user", content: "keep me" }),
      "{ not valid json",
      JSON.stringify({ role: "system", content: "wrong role" }),
      JSON.stringify({ role: "assistant", content: 42 }),
      JSON.stringify({ role: "assistant", content: "keep me too" }),
    ];
    await writeFile(join(dir, FILE), lines.join("\n") + "\n");
    assert.deepEqual(await loadTranscript(dir, ID), [
      { role: "user", content: "keep me" },
      { role: "assistant", content: "keep me too" },
    ]);
  } finally {
    await rm(dir, { recursive: true, force: true });
  }
});

test("drops a trailing lone user record (torn write)", async () => {
  const dir = await tmpDir();
  try {
    const lines = [
      JSON.stringify({ role: "user", content: "q1" }),
      JSON.stringify({ role: "assistant", content: "a1" }),
      JSON.stringify({ role: "user", content: "q2 with no reply" }),
    ];
    await writeFile(join(dir, FILE), lines.join("\n") + "\n");
    assert.deepEqual(await loadTranscript(dir, ID), [
      { role: "user", content: "q1" },
      { role: "assistant", content: "a1" },
    ]);
  } finally {
    await rm(dir, { recursive: true, force: true });
  }
});

test("preserves embedded newlines in content", async () => {
  const dir = await tmpDir();
  try {
    await createTranscript(dir, ID);
    await appendTurn(dir, ID, "line1\nline2", "reply\nwith\nnewlines");
    assert.deepEqual(await loadTranscript(dir, ID), [
      { role: "user", content: "line1\nline2" },
      { role: "assistant", content: "reply\nwith\nnewlines" },
    ]);
  } finally {
    await rm(dir, { recursive: true, force: true });
  }
});

test("creating an existing native ID cannot erase its history", async () => {
  const dir = await tmpDir();
  try {
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
  } finally {
    await rm(dir, { recursive: true, force: true });
  }
});
