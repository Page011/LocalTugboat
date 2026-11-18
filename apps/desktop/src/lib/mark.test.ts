import { describe, expect, test } from "bun:test";
import { readFileSync } from "node:fs";
import { join } from "node:path";

// Mark.svelte carries the logo's path data inline. It must stay an exact copy of assets/brand/tugboat-logo.svg:
// the leftmost porthole is cut open by the hull's bottom edge ON PURPOSE (a signature detail of the design), so any
// "tidied" or re-traced geometry fails here.
const brand = join(import.meta.dir, "../../../../assets/brand");
const norm = (d: string) => d.replace(/\s+/g, " ").trim();
const pathData = (text: string) => [...text.matchAll(/<path\b[^>]*?\sd="([^"]*)"/g)].map((m) => norm(m[1]!));

describe("Mark.svelte", () => {
  const svg = readFileSync(join(brand, "tugboat-logo.svg"), "utf8");
  const mark = readFileSync(join(import.meta.dir, "Mark.svelte"), "utf8");

  test("has the same four paths as the kit's logo, character for character", () => {
    const kit = pathData(svg);
    expect(kit.length).toBe(4);
    expect(pathData(mark)).toEqual(kit);
  });

  test("keeps the kit's viewBox and fill rule", () => {
    expect(svg).toContain('viewBox="0 0 904 362"');
    expect(mark).toContain('viewBox="0 0 904 362"');
    expect(mark.match(/fill-rule="evenodd"/g)?.length).toBe(4);
  });

  test("the hull still carries all four portholes, the leftmost one included (3 circles + the cut-open one in the outline)", () => {
    // Three portholes are separate circle sub-paths (24.87 radius arcs); the leftmost is part of the hull outline itself.
    const hull = pathData(mark)[2]!;
    expect(hull.match(/A24\.87 24\.87/g)?.length).toBe(6); // each circle = two arcs
    expect(hull.startsWith("M298.63 272.24")).toBe(true);
  });

  test("is a plain, decorative svg: no script, no raster, no external reference", () => {
    expect(mark).toContain('aria-hidden="true"');
    expect(mark).not.toMatch(/<image|href=|xlink|<use|<filter/);
  });
});
