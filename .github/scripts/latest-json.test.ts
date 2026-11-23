import { describe, expect, test } from "bun:test";
import { existsSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { build, installerName, type Inputs } from "./latest-json";

// A real minisign signature made with `tauri signer sign --app-version 0.4.0` and a throwaway key, over PAYLOAD.
const SIGNATURE =
  "dW50cnVzdGVkIGNvbW1lbnQ6IHNpZ25hdHVyZSBmcm9tIHRhdXJpIHNlY3JldCBrZXkKUlVRbVl0WTluQkE2MHVvZGtISU5WQXQrS2QwbUZiSWwwRElGc0dWTVE4OTF6RHVUdm9JNTlRV3hnallFY2ZRWUVhT1BuVjlZUElpdVdFT29zWncxN2RZOEZyTXJWMGpHbmdVPQp0cnVzdGVkIGNvbW1lbnQ6IHRpbWVzdGFtcDoxNzk1NDcwMDcwCWZpbGU6cGF5bG9hZC5iaW4JdmVyc2lvbjowLjQuMApDWUVjbVFHRHhtV3cvbUoxMVNNc1ArRmZic0JWcCs2dDQxcE1ZRVR3RWR1UEtYSFY0eE1UOEVoQ0hxcHRwc3A2YkVFU1NyMXhSaTFoVnArQTRsTnJEZz09Cg==";

// The bytes that were signed, and the public key (the .pub file, base64) of the throwaway key that signed them.
const PAYLOAD = Buffer.from("Not a real installer: bytes for the updater signature test.");
const PUBKEY =
  "dW50cnVzdGVkIGNvbW1lbnQ6IG1pbmlzaWduIHB1YmxpYyBrZXk6IEQyM0ExMDlDM0RENjYyMjYKUldRbVl0WTluQkE2MHBHSWZiK0lwY3gzRVpkQVVPNzkyU3ZKMXBvV0dUY0QwSDFoQUVOV1V2S1oK";
// The public key of a second throwaway key, as if the owner had run `signer generate` twice.
const OTHER_PUBKEY =
  "dW50cnVzdGVkIGNvbW1lbnQ6IG1pbmlzaWduIHB1YmxpYyBrZXk6IDk1RjZBNUFCRERCNjAxRDIKUldUU0FiYmRxNlgybFNnS3VldWU0NXhhRmxZQ1Rodjk3K1JQVytBdm1HcEVkN0ZRcW5CMXd4K2gK";

const ok: Inputs = {
  version: "0.4.0",
  installer: installerName("0.4.0"),
  installerBytes: PAYLOAD,
  signature: SIGNATURE,
  pubkey: PUBKEY,
  repo: "Page011/LocalTugboat",
  date: "2026-11-24T10:00:00Z",
};

const b64 = (s: string) => Buffer.from(s, "utf8").toString("base64");

describe("latest.json", () => {
  test("has the shape the updater reads, with the signature of that exact file and the download URL of the release", () => {
    const m = JSON.parse(build(ok));
    expect(m).toEqual({
      version: "0.4.0",
      notes: "Local Tugboat 0.4.0 - https://github.com/Page011/LocalTugboat/releases/tag/v0.4.0",
      pub_date: "2026-11-24T10:00:00Z",
      platforms: {
        "windows-x86_64": {
          signature: SIGNATURE,
          url: "https://github.com/Page011/LocalTugboat/releases/download/v0.4.0/LocalTugboat_0.4.0_x64-setup.exe",
        },
      },
    });
    expect(m.notes).not.toContain("\n");
  });

  test("a .sig file's trailing newline is dropped, never written into the signature", () => {
    expect(JSON.parse(build({ ...ok, signature: `${SIGNATURE}\r\n` })).platforms["windows-x86_64"].signature).toBe(SIGNATURE);
  });

  test("a signature made for another version is refused", () => {
    expect(() => build({ ...ok, version: "0.4.1", installer: installerName("0.4.1") })).toThrow("made for version 0.4.0, not 0.4.1");
  });

  test("a signature that does not name its version, or is not a signature, is refused", () => {
    const unversioned = b64("untrusted comment: signature from tauri secret key\nRUQabc\ntrusted comment: timestamp:1\tfile:x\nabc\n");
    expect(() => build({ ...ok, signature: unversioned })).toThrow("does not say which version");
    expect(() => build({ ...ok, signature: b64("hello") })).toThrow("not a minisign signature");
    expect(() => build({ ...ok, signature: "" })).toThrow("empty");
    expect(() => build({ ...ok, signature: "two\nlines" })).toThrow("one line");
  });

  test("a signature made with another key than plugins.updater.pubkey is refused", () => {
    expect(() => build({ ...ok, pubkey: OTHER_PUBKEY })).toThrow("different key than plugins.updater.pubkey");
  });

  test("a signature that does not cover the installer bytes is refused", () => {
    expect(() => build({ ...ok, installerBytes: Buffer.from("Not a real installer: other bytes.") })).toThrow("does not match the installer");
    expect(() => build({ ...ok, installerBytes: Buffer.alloc(0) })).toThrow("does not match the installer");
  });

  test("a signature with the right key id but forged bytes is refused", () => {
    const lines = Buffer.from(SIGNATURE, "base64").toString("utf8").split("\n");
    const sig = Buffer.from(lines[1]!, "base64");
    sig[40] = sig[40]! ^ 1;
    lines[1] = sig.toString("base64");
    expect(() => build({ ...ok, signature: b64(lines.join("\n")) })).toThrow("does not match the installer");
  });

  test("an altered trusted comment is refused (the signed comment carries the version)", () => {
    const text = Buffer.from(SIGNATURE, "base64").toString("utf8").replace("file:payload.bin", "file:payload.exe");
    expect(() => build({ ...ok, signature: b64(text) })).toThrow("trusted comment was altered");
  });

  test("a public key that is not a minisign public key is refused", () => {
    expect(() => build({ ...ok, pubkey: "UPDATER_PUBKEY_MISSING" })).toThrow("public key");
    expect(() => build({ ...ok, pubkey: b64("untrusted comment: x\nnot-a-key\n") })).toThrow("public key");
    expect(() => build({ ...ok, pubkey: "" })).toThrow("public key");
  });

  test("the installer name, version, repository and date are checked", () => {
    expect(() => build({ ...ok, installer: "Local Tugboat_0.4.0_x64-setup.exe" })).toThrow("must be named LocalTugboat_0.4.0_x64-setup.exe");
    expect(() => build({ ...ok, version: "v0.4.0" })).toThrow("bad version");
    expect(() => build({ ...ok, version: "null" })).toThrow("bad version");
    expect(() => build({ ...ok, repo: "Page011" })).toThrow("bad repository");
    expect(() => build({ ...ok, date: "yesterday" })).toThrow("bad date");
    expect(() => build({ ...ok, date: "2026-13-45T10:00:00Z" })).toThrow("bad date");
  });

  test("the script runs on files: writes the manifest, and on bad input writes nothing and exits 1", () => {
    const dir = mkdtempSync(join(tmpdir(), "latest-json-"));
    try {
      const sig = join(dir, "setup.exe.sig");
      writeFileSync(sig, `${SIGNATURE}\n`);
      const exe = join(dir, installerName("0.4.0"));
      writeFileSync(exe, PAYLOAD);
      const script = join(import.meta.dir, "latest-json.ts");
      const run = (extra: string[], out: string) =>
        Bun.spawnSync([process.execPath, script, ...extra, "--version", "0.4.0", "--installer", installerName("0.4.0"), "--sig", sig, "--installer-file", exe, "--pubkey", PUBKEY, "--repo", "Page011/LocalTugboat", "--out", out]);

      const out = join(dir, "latest.json");
      const good = run(["--date", "2026-11-24T10:00:00Z"], out);
      expect(good.exitCode).toBe(0);
      expect(JSON.parse(readFileSync(out, "utf8")).version).toBe("0.4.0");

      const withDefaultDate = join(dir, "dated.json");
      expect(run([], withDefaultDate).exitCode).toBe(0);
      expect(JSON.parse(readFileSync(withDefaultDate, "utf8")).pub_date).toMatch(/^\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}Z$/);

      // A key from another `signer generate`: the script fails and writes nothing.
      const mismatched = join(dir, "mismatched.json");
      const wrongKey = run(["--pubkey", OTHER_PUBKEY], mismatched);
      expect(wrongKey.exitCode).toBe(1);
      expect(wrongKey.stderr.toString()).toContain("different key than plugins.updater.pubkey");
      expect(existsSync(mismatched)).toBe(false);

      const bad = join(dir, "bad.json");
      const failed = run(["--date", "never"], bad);
      expect(failed.exitCode).toBe(1);
      expect(failed.stderr.toString()).toContain("bad date");
      expect(existsSync(bad)).toBe(false);
    } finally {
      rmSync(dir, { recursive: true, force: true });
    }
  });
});
