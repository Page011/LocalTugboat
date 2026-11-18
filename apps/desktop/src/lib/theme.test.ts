import { describe, expect, test } from "bun:test";
import { readdirSync, readFileSync, statSync } from "node:fs";
import { join } from "node:path";

const src = join(import.meta.dir, "..");
const css = readFileSync(join(src, "app.css"), "utf8");

function tokens(block: string): Record<string, string> {
  return Object.fromEntries([...block.matchAll(/(--[\w-]+):\s*(#[0-9a-f]{6})\s*;/gi)].map((m) => [m[1]!, m[2]!.toLowerCase()]));
}
// The light block is the first `:root {`; the dark one is the `:root {` inside the media query.
const lightBlock = css.slice(css.indexOf(":root {"), css.indexOf("@media (prefers-color-scheme: dark)"));
const darkBlock = css.slice(css.indexOf("@media (prefers-color-scheme: dark)"), css.indexOf("* {"));
const light = tokens(lightBlock);
const dark = tokens(darkBlock);

function luminance(hex: string): number {
  const c = [1, 3, 5].map((i) => parseInt(hex.slice(i, i + 2), 16) / 255).map((v) => (v <= 0.03928 ? v / 12.92 : ((v + 0.055) / 1.055) ** 2.4));
  return 0.2126 * c[0]! + 0.7152 * c[1]! + 0.0722 * c[2]!;
}
export function ratio(a: string, b: string): number {
  const [hi, lo] = [luminance(a), luminance(b)].sort((x, y) => y - x);
  return (hi! + 0.05) / (lo! + 0.05);
}

/** Text pairs: every combination the screens actually draw. */
const TEXT: Array<[string, string]> = [
  ["--fg", "--bg"], ["--fg", "--surface"], ["--fg", "--surface-2"], ["--fg", "--accent-tint"],
  ["--fg-muted", "--bg"], ["--fg-muted", "--surface"], ["--fg-muted", "--surface-2"], ["--fg-muted", "--accent-tint"],
  ["--accent", "--surface"], ["--accent", "--bg"], ["--on-accent", "--accent"],
  ["--ok", "--ok-bg"], ["--ok", "--surface"], ["--ok", "--bg"], ["--warn", "--warn-bg"], ["--warn", "--surface"],
  ["--danger", "--danger-bg"], ["--danger", "--surface"], ["--danger", "--bg"], ["--on-danger", "--danger-solid"],
  ["--accent", "--surface-2"], ["--on-action", "--action"],
  // The navy rail: its text, muted text, hover and current-screen fills.
  ["--rail-fg", "--rail"], ["--rail-muted", "--rail"], ["--rail-fg", "--rail-hover"], ["--rail-muted", "--rail-hover"],
  ["--rail-fg", "--rail-current"],
];
/** Non-text: control edges and the focus ring need 3:1 (WCAG 1.4.11). */
const UI: Array<[string, string]> = [
  ["--border-strong", "--surface"], ["--border-strong", "--bg"], ["--accent", "--surface"], ["--accent", "--bg"],
  ["--danger-solid", "--surface"], ["--danger-solid", "--bg"],
  ["--action", "--surface"], ["--action", "--bg"], ["--rail-edge", "--rail"], ["--rail-buoy", "--rail"],
  // The logo above "Nothing yet." (decorative, but it must show): navy on light, white on dark.
  ["--logo-ink", "--bg"], ["--logo-ink", "--surface"],
];

for (const [name, t] of [["light", light], ["dark", dark]] as const) {
  describe(`${name} theme contrast`, () => {
    test("every token the pairs use exists", () => {
      for (const [a, b] of [...TEXT, ...UI]) {
        expect(t[a], a).toBeDefined();
        expect(t[b], b).toBeDefined();
      }
    });
    test.each(TEXT)("text %s on %s is at least 4.5:1", (a, b) => {
      const r = ratio(t[a]!, t[b]!);
      if (process.env.SHOW_RATIOS) console.log(`${name} ${a}/${b} ${r.toFixed(1)}`);
      expect(r).toBeGreaterThanOrEqual(4.5);
    });
    test.each(UI)("edge %s on %s is at least 3:1", (a, b) => {
      const r = ratio(t[a]!, t[b]!);
      if (process.env.SHOW_RATIOS) console.log(`${name} ${a}/${b} ${r.toFixed(1)}`);
      expect(r).toBeGreaterThanOrEqual(3);
    });
  });
}

describe("the logo colours (assets/brand/README.md)", () => {
  test("ink is the kit's navy on light and white on dark; the rail logo is white on the navy rail", () => {
    expect(light["--logo-ink"]).toBe("#1a2733");
    expect(dark["--logo-ink"]).toBe("#ffffff");
    const app = readFileSync(join(src, "App.svelte"), "utf8");
    expect(app).toMatch(/--mark-fg:\s*#ffffff/);
    expect(ratio("#ffffff", light["--rail"]!)).toBeGreaterThanOrEqual(3);
    expect(ratio("#ffffff", dark["--rail"]!)).toBeGreaterThanOrEqual(3);
  });
});

describe("the rail and the danger colour", () => {
  test("the dark rail is a visibly separate navy rail (>= 1.2:1 off the page)", () => {
    expect(ratio(dark["--rail"]!, dark["--bg"]!)).toBeGreaterThanOrEqual(1.2);
    expect(ratio(light["--rail"]!, light["--bg"]!)).toBeGreaterThanOrEqual(1.2);
    // Hover and current fills must also stand off the rail.
    for (const k of ["--rail-hover", "--rail-current"]) expect(ratio(dark[k]!, dark["--rail"]!)).toBeGreaterThanOrEqual(1.2);
  });
  test("danger text is red in both themes, never pink or plum (urgent is red)", () => {
    for (const t of [light, dark]) {
      const [r, g, b] = [1, 3, 5].map((i) => parseInt(t["--danger"]!.slice(i, i + 2), 16));
      expect(r!).toBeGreaterThanOrEqual(g! + 60);
      expect(r!).toBeGreaterThanOrEqual(b! + 60);
      expect(g!).toBeLessThan(0xa0);
      expect(b!).toBeLessThan(0xa0);
    }
  });
});

describe("both themes define the same tokens", () => {
  test("no token exists in only one theme", () => {
    expect(Object.keys(dark).filter((k) => k !== "--scrim").sort()).toEqual(Object.keys(light).filter((k) => k !== "--scrim").sort());
  });
});

// The standing UI rules, as a source scan: a one-edge accent stripe is never allowed.
describe("no accent stripes", () => {
  const files: string[] = [];
  (function walk(dir: string) {
    for (const f of readdirSync(dir)) {
      const p = join(dir, f);
      if (statSync(p).isDirectory()) f !== "bindings" && walk(p);
      else if (/\.(svelte|css)$/.test(f)) files.push(p);
    }
  })(src);

  test("scan finds the sources", () => expect(files.length).toBeGreaterThan(1));
  test.each(files.map((f) => [f.slice(src.length + 1), f] as const))("%s has no one-edge border, inset bar or ::before bar", (_n, f) => {
    const text = readFileSync(f, "utf8");
    expect(text).not.toMatch(/border-(left|right|top|bottom)\s*:\s*[2-9]px/);
    expect(text).not.toMatch(/inset\s+-?\d+px\s+0\s+0\s+0?\s*(var|#|rgb)/);
    expect(text).not.toMatch(/::before\s*\{[^}]*(width|height)\s*:\s*[2-9]px/);
  });
});
