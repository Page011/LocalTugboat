import { describe, expect, test } from "bun:test";
import { readFileSync } from "node:fs";
import { join } from "node:path";

interface Step {
  name?: string;
  if?: string;
  env?: Record<string, string>;
  run?: string;
}

const text = readFileSync(join(import.meta.dir, "..", "workflows", "release.yml"), "utf8");
const wf = Bun.YAML.parse(text) as { jobs: Record<string, { steps: Step[] }> };
const steps = wf.jobs.build!.steps;
const step = (name: string): Step => {
  const s = steps.find((x) => x.name === name);
  if (!s) throw new Error(`no step named ${name}`);
  return s;
};
const TAG_ONLY = "startsWith(github.ref, 'refs/tags/v')";

describe("release workflow", () => {
  test("the signing key and its password are given to tag runs only, and only to the signing step", () => {
    const withSecrets = steps.filter((s) => JSON.stringify(s.env ?? {}).includes("TAURI_SIGNING_PRIVATE_KEY"));
    expect(withSecrets.map((s) => s.name)).toEqual(["Update signing is set up", "Sign installer"]);
    for (const s of withSecrets) expect(s.if).toBe(TAG_ONLY);
    // Nothing else in the build mentions the secrets, not even through a job or workflow level env.
    expect(text.match(/secrets\.TAURI_SIGNING_PRIVATE_KEY(_PASSWORD)?/g)).toHaveLength(4);
  });

  test("the build runs without the key and without making updater artifacts", () => {
    const build = step("Build installer");
    expect(build.env).toBeUndefined();
    expect(build.run).toContain(`--config '{"bundle":{"createUpdaterArtifacts":false}}'`);
  });

  test("signing and latest.json happen on tag runs only, so other runs upload an unsigned installer and no manifest", () => {
    expect(step("Sign installer").if).toBe(TAG_ONLY);
    expect(step("Write latest.json").if).toBe(TAG_ONLY);
    expect(step("Sign installer").run).toContain("signer sign --app-version");
  });

  test("latest.json is checked against the configured key and the installer's bytes", () => {
    const run = step("Write latest.json").run ?? "";
    expect(run).toContain("--pubkey");
    expect(run).toContain("plugins.updater.pubkey");
    expect(run).toContain("--installer-file");
  });

  test("a tag with a suffix is published as a pre-release", () => {
    const release = wf.jobs.release!.steps.find((s) => s.name === "Create draft release");
    expect(release?.run).toContain("*-*) pre=(--prerelease)");
    expect(release?.run).toMatch(/gh release create .*\$\{pre\[@\]\+"\$\{pre\[@\]\}"\}/);
  });
});
