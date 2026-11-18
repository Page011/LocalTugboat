import { describe, expect, test } from "bun:test";
import { claimAll } from "./generation";

describe("claimAll", () => {
  test("an untouched claim checks true for every field", () => {
    const gen = { a: 0, b: 0 };
    const ok = claimAll(gen);
    expect(ok("a") && ok("b")).toBe(true);
  });
  test("a later claim invalidates the earlier one (overlapping reloads)", () => {
    const gen = { a: 0, b: 0 };
    const first = claimAll(gen);
    const second = claimAll(gen);
    expect(first("a")).toBe(false);
    expect(first("b")).toBe(false);
    expect(second("a") && second("b")).toBe(true);
  });
  test("an event or field loader bumping one counter fails only that field", () => {
    const gen = { a: 0, b: 0 };
    const ok = claimAll(gen);
    gen.a++;
    expect(ok("a")).toBe(false);
    expect(ok("b")).toBe(true);
  });
});
