// Client half of the live-view compressed frame stream (see the `caps`
// entry in src/server/live_ws.rs). The server sends binary WS messages
// carrying one connection-lifetime raw-deflate stream, sync-flushed per
// frame; the decompressed plaintext is a sequence of `u32-LE length ||
// frame JSON` records. One stream rather than per-message compression on
// purpose: consecutive frames are near-identical, so the shared dictionary
// turns each into back-references (delta encoding without diff
// bookkeeping), which is what keeps 60fps scroll bursts to a few hundred
// bytes per frame.

/** True when this browser can inflate the compressed frame stream; gates
 *  the client's `caps` advertisement, so unsupported browsers (and jsdom)
 *  simply keep receiving JSON text frames. */
export function supportsFrameDeflate(): boolean {
  return typeof DecompressionStream === "function";
}

export interface FrameInflater {
  /** Feed one binary WS message's bytes. Ordering is the caller's WS
   *  message order; the stream is inherently sequential. */
  push(chunk: ArrayBuffer): void;
  /** Tear down the stream (connection closed / hook unmounted). */
  dispose(): void;
}

/**
 * One inflater per WS connection. `onFrame` receives each decoded frame's
 * JSON text, in order. `onError` fires once on a corrupt stream (bad
 * record framing, inflate failure); the caller should drop the connection
 * and let its reconnect machinery redial, since a mid-stream inflate state
 * is unrecoverable.
 */
export function createFrameInflater(onFrame: (json: string) => void, onError: (err: unknown) => void): FrameInflater {
  const stream = new DecompressionStream("deflate-raw");
  const writer = stream.writable.getWriter();
  const reader = stream.readable.getReader();
  const decoder = new TextDecoder();
  let buf = new Uint8Array(0);
  let failed = false;
  const fail = (err: unknown) => {
    if (failed) return;
    failed = true;
    onError(err);
  };

  void (async () => {
    try {
      for (;;) {
        const { done, value } = await reader.read();
        if (done) return;
        if (buf.length === 0) {
          buf = value;
        } else {
          const next = new Uint8Array(buf.length + value.length);
          next.set(buf, 0);
          next.set(value, buf.length);
          buf = next;
        }
        // Drain complete records; a record split across inflate chunks
        // just waits for the rest.
        let pos = 0;
        while (buf.length - pos >= 4) {
          const len = new DataView(buf.buffer, buf.byteOffset + pos, 4).getUint32(0, true);
          if (buf.length - pos - 4 < len) break;
          onFrame(decoder.decode(buf.subarray(pos + 4, pos + 4 + len)));
          pos += 4 + len;
        }
        buf = pos > 0 ? buf.slice(pos) : buf;
      }
    } catch (err) {
      fail(err);
    }
  })();

  return {
    push(chunk: ArrayBuffer) {
      // Writes queue in order on the stream's internal queue; per-write
      // await would serialize against inflate for no benefit.
      writer.write(new Uint8Array(chunk)).catch(fail);
    },
    dispose() {
      failed = true; // silence teardown-race errors from the reader loop
      writer.abort().catch(() => {});
      reader.cancel().catch(() => {});
    },
  };
}
