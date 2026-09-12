// @vitest-environment node
//
// Client half of the compressed live-frame stream. The server side is a
// connection-lifetime raw-deflate stream sync-flushed per frame
// (FrameDeflater in src/server/live_ws.rs, unit-tested there); node's zlib
// speaks the same format, so these tests produce real sync-flushed chunks
// and assert the inflater re-splits the plaintext records correctly.

import { describe, expect, it, vi } from "vitest";
import zlib from "node:zlib";
import { createFrameInflater, supportsFrameDeflate } from "./frameStream";

function toArrayBuffer(u8: Uint8Array): ArrayBuffer {
  return u8.buffer.slice(u8.byteOffset, u8.byteOffset + u8.byteLength) as ArrayBuffer;
}

/** Streaming raw-deflate that emits one sync-flushed chunk per frame,
 *  mirroring the server's FrameDeflater (shared dictionary across calls). */
function makeDeflater() {
  const stream = zlib.createDeflateRaw();
  const pending: Buffer[] = [];
  stream.on("data", (c: Buffer) => pending.push(c));
  return (json: string): Promise<Uint8Array> => {
    const body = Buffer.from(json, "utf8");
    const len = Buffer.alloc(4);
    len.writeUInt32LE(body.length, 0);
    stream.write(Buffer.concat([len, body]));
    return new Promise((resolve) => {
      stream.flush(zlib.constants.Z_SYNC_FLUSH, () => {
        resolve(new Uint8Array(Buffer.concat(pending.splice(0))));
      });
    });
  };
}

describe("frameStream", () => {
  it("advertises support where DecompressionStream exists", () => {
    expect(supportsFrameDeflate()).toBe(true);
  });

  it("decodes sequential frames in order through one stream", async () => {
    const deflate = makeDeflater();
    const frames: string[] = [];
    const onError = vi.fn();
    const inflater = createFrameInflater((f) => frames.push(f), onError);
    const f1 = JSON.stringify({ type: "frame", content: "line one\n".repeat(50), rows: 24 });
    const f2 = JSON.stringify({ type: "frame", content: "line one\n".repeat(49) + "line two\n", rows: 24 });
    inflater.push(toArrayBuffer(await deflate(f1)));
    inflater.push(toArrayBuffer(await deflate(f2)));
    await vi.waitFor(() => expect(frames).toEqual([f1, f2]));
    expect(onError).not.toHaveBeenCalled();
    inflater.dispose();
  });

  it("reassembles a record split across pushed chunks", async () => {
    const deflate = makeDeflater();
    const frames: string[] = [];
    const inflater = createFrameInflater(
      (f) => frames.push(f),
      () => {},
    );
    const f1 = JSON.stringify({ type: "frame", content: "x".repeat(4000) });
    const compressed = await deflate(f1);
    const mid = Math.floor(compressed.length / 2);
    inflater.push(toArrayBuffer(compressed.subarray(0, mid)));
    inflater.push(toArrayBuffer(compressed.subarray(mid)));
    await vi.waitFor(() => expect(frames).toEqual([f1]));
    inflater.dispose();
  });

  it("reports a corrupt stream once via onError", async () => {
    const onError = vi.fn();
    const inflater = createFrameInflater(() => {}, onError);
    inflater.push(toArrayBuffer(new Uint8Array([0xff, 0xff, 0xff, 0xff, 0x00, 0x01, 0x02])));
    await vi.waitFor(() => expect(onError).toHaveBeenCalledTimes(1));
    inflater.dispose();
  });

  it("dispose silences teardown races instead of surfacing them as errors", async () => {
    for (const failure of ["read", "write"] as const) {
      let resolveRead!: (value: ReadableStreamReadResult<Uint8Array>) => void;
      let rejectRead!: (error: Error) => void;
      const read = new Promise<ReadableStreamReadResult<Uint8Array>>((resolve, reject) => {
        resolveRead = resolve;
        rejectRead = reject;
      });
      let resolveWrite!: () => void;
      let rejectWrite!: (error: Error) => void;
      const write = new Promise<void>((resolve, reject) => {
        resolveWrite = resolve;
        rejectWrite = reject;
      });
      const reader = { read: vi.fn(() => read), cancel: vi.fn().mockResolvedValue(undefined) };
      const writer = { write: vi.fn(() => write), abort: vi.fn().mockResolvedValue(undefined) };
      vi.stubGlobal(
        "DecompressionStream",
        class {
          readable = { getReader: () => reader };
          writable = { getWriter: () => writer };
        },
      );
      try {
        const onError = vi.fn();
        const inflater = createFrameInflater(() => {}, onError);
        inflater.push(new ArrayBuffer(1));
        expect(reader.read).toHaveBeenCalledTimes(1);
        expect(writer.write).toHaveBeenCalledTimes(1);
        inflater.dispose();

        // Release the pending operation after disposal, then join its error handler.
        if (failure === "read") {
          rejectRead(new Error("cancelled read"));
          resolveWrite();
        } else {
          resolveRead({ done: true, value: undefined });
          rejectWrite(new Error("aborted write"));
        }
        await Promise.allSettled([read, write]);
        expect(onError, failure).not.toHaveBeenCalled();
      } finally {
        vi.unstubAllGlobals();
      }
    }
  });
});
