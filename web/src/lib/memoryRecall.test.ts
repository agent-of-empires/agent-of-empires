import { expect, it } from "vitest";
import { asMemoryRecall, pickMemoryRecall } from "./memoryRecall";

it("asMemoryRecall keeps well-typed fields and rejects a malformed payload", () => {
  const cases: [unknown, unknown][] = [
    [
      { mode: "synthesize", synthesized_text: "hi" },
      { mode: "synthesize", synthesized_text: "hi" },
    ],
    [
      { mode: "recall", paths: ["/a.md", "/b.md"] },
      { mode: "recall", paths: ["/a.md", "/b.md"] },
    ],
    [{ mode: "recall", paths: ["/a.md", 3] }, { mode: "recall" }],
    [{ mode: "synthesize", synthesized_text: 42 }, { mode: "synthesize" }],
    [{ synthesized_text: "hi" }, undefined],
    [{ mode: 1 }, undefined],
    [null, undefined],
    [undefined, undefined],
    ["x", undefined],
    [[{ mode: "recall" }], undefined],
  ];
  for (const [input, expected] of cases) expect(asMemoryRecall(input), JSON.stringify(input)).toEqual(expected);
});

it("pickMemoryRecall reads parsed args, then raw argsText, else undefined", () => {
  const recall = { mode: "recall", paths: ["/a.md"] };
  expect(pickMemoryRecall({ _aoe_memory_recall: recall }, undefined)).toEqual(recall);
  expect(pickMemoryRecall(undefined, JSON.stringify({ _aoe_memory_recall: recall }))).toEqual(recall);
  expect(pickMemoryRecall(undefined, undefined)).toBeUndefined();
  expect(pickMemoryRecall({}, "{}")).toBeUndefined();
  expect(pickMemoryRecall(undefined, "not json")).toBeUndefined();
  expect(pickMemoryRecall({ _aoe_memory_recall: { synthesized_text: "x" } }, undefined)).toBeUndefined();
});
