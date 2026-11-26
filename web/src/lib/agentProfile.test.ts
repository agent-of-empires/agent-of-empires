import { expect, it } from "vitest";
import { isClearAlias } from "./agentProfile";

it.each<[string, string[], boolean]>([
  ["/clear", ["/clear"], true],
  ["/new", ["/new"], true],
  ["  /clear  ", ["/clear"], true],
  ["\n/clear\n", ["/clear"], true],
  ["/clear --hard", ["/clear"], true],
  ["clear", ["/clear"], false],
  ["/cleart", ["/clear"], false],
  ["hello /clear world", ["/clear"], false],
  ["", ["/clear"], false],
  ["/clear", [], false],
  ["/new", ["/clear"], false],
])("isClearAlias(%j, %j) is %s", (text, aliases, expected) => {
  expect(isClearAlias(text, aliases)).toBe(expected);
});
