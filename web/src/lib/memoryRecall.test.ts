import { describe, expect, it } from "vitest";
import { asMemoryRecall, pickMemoryRecall } from "./memoryRecall";

describe("asMemoryRecall", () => {
  it("accepts a well-formed synthesize payload", () => {
    expect(asMemoryRecall({ mode: "synthesize", synthesized_text: "hi" })).toEqual({
      mode: "synthesize",
      synthesized_text: "hi",
    });
  });

  it("accepts a recall payload with string paths", () => {
    expect(asMemoryRecall({ mode: "recall", paths: ["/a.md", "/b.md"] })).toEqual({
      mode: "recall",
      paths: ["/a.md", "/b.md"],
    });
  });

  it("drops non-string paths entries", () => {
    expect(asMemoryRecall({ mode: "recall", paths: ["/a.md", 3] })).toEqual({ mode: "recall" });
  });

  it("drops a non-string synthesized_text", () => {
    expect(asMemoryRecall({ mode: "synthesize", synthesized_text: 42 })).toEqual({ mode: "synthesize" });
  });

  it("rejects a missing or non-string mode", () => {
    expect(asMemoryRecall({ synthesized_text: "hi" })).toBeUndefined();
    expect(asMemoryRecall({ mode: 1 })).toBeUndefined();
  });

  it("rejects non-objects, null, and arrays", () => {
    expect(asMemoryRecall(null)).toBeUndefined();
    expect(asMemoryRecall(undefined)).toBeUndefined();
    expect(asMemoryRecall("x")).toBeUndefined();
    expect(asMemoryRecall([{ mode: "recall" }])).toBeUndefined();
  });
});

describe("pickMemoryRecall", () => {
  it("reads from the parsed args object", () => {
    expect(pickMemoryRecall({ _aoe_memory_recall: { mode: "synthesize", synthesized_text: "x" } }, undefined)).toEqual({
      mode: "synthesize",
      synthesized_text: "x",
    });
  });

  it("falls back to the raw argsText JSON", () => {
    const argsText = JSON.stringify({ _aoe_memory_recall: { mode: "recall", paths: ["/a.md"] } });
    expect(pickMemoryRecall(undefined, argsText)).toEqual({ mode: "recall", paths: ["/a.md"] });
  });

  it("returns undefined when neither source carries it", () => {
    expect(pickMemoryRecall(undefined, undefined)).toBeUndefined();
    expect(pickMemoryRecall({}, "{}")).toBeUndefined();
  });

  it("returns undefined on malformed argsText", () => {
    expect(pickMemoryRecall(undefined, "not json")).toBeUndefined();
  });

  it("returns undefined when the smuggled value is malformed", () => {
    expect(pickMemoryRecall({ _aoe_memory_recall: { synthesized_text: "x" } }, undefined)).toBeUndefined();
  });
});
