import { test } from "node:test";
import assert from "node:assert/strict";
import { createAgentStream } from "../src/transport.ts";

test("accepts large tool results without losing the next frame and retains the frame cap", async () => {
  const content = "x".repeat(33 * 1024 * 1024);
  const result = { jsonrpc: "2.0", id: 1, result: { content } };
  const next = { jsonrpc: "2.0", id: 2, result: {} };
  const input = new ReadableStream<Uint8Array>({
    start(controller) {
      const encoder = new TextEncoder();
      controller.enqueue(encoder.encode(JSON.stringify(result) + "\n"));
      controller.enqueue(encoder.encode(JSON.stringify(next) + "\n"));
      controller.close();
    },
  });
  const reader = createAgentStream(new WritableStream(), input).readable.getReader();
  try {
    assert.deepEqual(await reader.read(), { value: result, done: false });
    assert.deepEqual(await reader.read(), { value: next, done: false });
    assert.equal((await reader.read()).done, true);
  } finally {
    reader.releaseLock();
  }

  const oversized = new ReadableStream<Uint8Array>({
    start(controller) {
      const chunk = new Uint8Array(1024 * 1024).fill(120);
      for (let i = 0; i < 65; i++) controller.enqueue(chunk);
      controller.close();
    },
  });
  const bounded = createAgentStream(new WritableStream(), oversized).readable.getReader();
  try {
    await assert.rejects(bounded.read(), { name: "MessageTooLargeError" });
  } finally {
    bounded.releaseLock();
  }
});
