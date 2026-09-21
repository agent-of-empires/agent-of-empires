// Client half of the live-view compressed frame stream (see `caps` in src/server/live_ws.rs): one raw-deflate stream per connection of `u32-LE length || JSON` records, so near-identical frames compress to back-references.

/** Gates the `caps` advertisement; others keep JSON text frames. */
export function supportsFrameDeflate(): boolean {
  return typeof DecompressionStream === "function";
}

export interface FrameInflater {
  /** Must be called in WS message order. */
  push(chunk: ArrayBuffer): void;
  dispose(): void;
}

/** `onError` fires once on a corrupt stream; the caller should reconnect. */
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
        // A record split across inflate chunks waits for the rest.
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
      // Writes queue in order internally.
      writer.write(new Uint8Array(chunk)).catch(fail);
    },
    dispose() {
      failed = true; // silence teardown-race errors from the reader loop
      writer.abort().catch(() => {});
      reader.cancel().catch(() => {});
    },
  };
}
