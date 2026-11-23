// Writes latest.json, the file the desktop app's update check reads from the latest GitHub release.
//
//   bun .github/scripts/latest-json.ts --version 0.3.1 --installer LocalTugboat_0.3.1_x64-setup.exe \
//       --installer-file out/LocalTugboat_0.3.1_x64-setup.exe --pubkey "<plugins.updater.pubkey>" \
//       --sig "Local Tugboat_0.3.1_x64-setup.exe.sig" --repo Page011/LocalTugboat --out out/latest.json
//
// --sig is the .sig that `tauri signer sign` wrote for the installer. A signature covers the file's bytes, not its
// name, so the installer can be renamed after the build (the release workflow does) and keep working.
// --installer is the published file name; --installer-file is the local file the signature is checked against.
// --pubkey is plugins.updater.pubkey from tauri.conf.json: the signature must verify against it, because an
// installed app would reject any other and never update. --date is optional (RFC 3339, default now).
// Any problem with the inputs is an error and writes nothing.

import { createHash, createPublicKey, verify } from "node:crypto";
import { readFileSync, writeFileSync } from "node:fs";

const SEMVER = /^\d+\.\d+\.\d+(-[0-9A-Za-z.-]+)?$/;
const REPO = /^[A-Za-z0-9_.-]+\/[A-Za-z0-9_.-]+$/;
const RFC3339 = /^\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}(\.\d+)?(Z|[+-]\d{2}:\d{2})$/;
const BASE64 = /^[A-Za-z0-9+/]+={0,2}$/;
const TRUSTED_PREFIX = "trusted comment: ";

export interface Inputs {
  version: string;
  /** File name of the installer as published (no folder). */
  installer: string;
  /** The installer's bytes: what the signature must verify against. */
  installerBytes: Uint8Array;
  /** Contents of the .sig file. */
  signature: string;
  /** plugins.updater.pubkey: the base64 of the .pub file. */
  pubkey: string;
  /** owner/name of the GitHub repository. */
  repo: string;
  /** RFC 3339. */
  date: string;
}

export function installerName(version: string): string {
  return `LocalTugboat_${version}_x64-setup.exe`;
}

function decode(text: string, what: string): Buffer {
  if (!BASE64.test(text)) throw new Error(`${what} is not base64`);
  return Buffer.from(text, "base64");
}

/** A minisign key line: 2 algorithm bytes, an 8 byte key id, then the 32 byte Ed25519 public key. */
function parsePublicKey(pubkey: string): { keyId: Buffer; raw: Buffer } {
  const lines = decode(pubkey.trim(), "the public key").toString("utf8").split(/\r?\n/);
  if (!lines[0]?.startsWith("untrusted comment:") || !lines[1]) throw new Error("plugins.updater.pubkey is not a minisign public key");
  const bytes = decode(lines[1].trim(), "the public key line");
  if (bytes.length !== 42 || bytes.subarray(0, 2).toString("latin1") !== "Ed") throw new Error("plugins.updater.pubkey is not a minisign public key");
  return { keyId: bytes.subarray(2, 10), raw: bytes.subarray(10) };
}

/**
 * The signature must be a minisign signature made for exactly this version, with the key in plugins.updater.pubkey,
 * over exactly these installer bytes. Minisign "prehashed" form (what tauri writes): Ed25519 over the BLAKE2b-512 of
 * the file, then a second Ed25519 signature over the first one plus the trusted comment.
 */
function checkSignature(signature: string, version: string, pubkey: string, installerBytes: Uint8Array): void {
  if (!signature) throw new Error("the signature is empty");
  if (/\s/.test(signature)) throw new Error("the signature must be one line (the contents of the .sig file)");
  if (!BASE64.test(signature)) throw new Error("the signature is not base64");
  const lines = Buffer.from(signature, "base64").toString("utf8").split(/\r?\n/);
  if (!lines[0]?.startsWith("untrusted comment:")) throw new Error("the signature is not a minisign signature");
  const trusted = lines.find((l) => l.startsWith(TRUSTED_PREFIX));
  if (!trusted) throw new Error("the signature has no trusted comment");
  const signed = trusted.split("\t").find((f) => f.startsWith("version:"))?.slice("version:".length);
  if (signed === undefined) throw new Error("the signature does not say which version it was made for (sign with --app-version)");
  if (signed !== version) throw new Error(`the signature was made for version ${signed}, not ${version}`);

  const key = parsePublicKey(pubkey);
  const sig = decode((lines[1] ?? "").trim(), "the signature line");
  const global = decode((lines[3] ?? "").trim(), "the trusted comment signature");
  if (sig.length !== 74 || sig.subarray(0, 2).toString("latin1") !== "ED" || global.length !== 64) throw new Error("the signature is not a prehashed minisign signature");
  if (!sig.subarray(2, 10).equals(key.keyId)) throw new Error("the signature was made with a different key than plugins.updater.pubkey");
  const publicKey = createPublicKey({ key: { kty: "OKP", crv: "Ed25519", x: key.raw.toString("base64url") }, format: "jwk" });
  const digest = createHash("blake2b512").update(installerBytes).digest();
  if (!verify(null, digest, publicKey, sig.subarray(10))) throw new Error("the signature does not match the installer (other bytes, or a different key than plugins.updater.pubkey)");
  const signedComment = Buffer.concat([sig.subarray(10), Buffer.from(trusted.slice(TRUSTED_PREFIX.length), "utf8")]);
  if (!verify(null, signedComment, publicKey, global)) throw new Error("the signature's trusted comment was altered");
}

export function build(i: Inputs): string {
  if (!SEMVER.test(i.version)) throw new Error(`bad version '${i.version}'`);
  if (i.installer !== installerName(i.version)) throw new Error(`the installer must be named ${installerName(i.version)}, got '${i.installer}'`);
  if (!REPO.test(i.repo)) throw new Error(`bad repository '${i.repo}' (want owner/name)`);
  if (!RFC3339.test(i.date) || Number.isNaN(Date.parse(i.date))) throw new Error(`bad date '${i.date}' (want RFC 3339)`);
  const signature = i.signature.trim();
  checkSignature(signature, i.version, i.pubkey, i.installerBytes);
  const manifest = {
    version: i.version,
    notes: `Local Tugboat ${i.version} - https://github.com/${i.repo}/releases/tag/v${i.version}`,
    pub_date: i.date,
    platforms: {
      "windows-x86_64": {
        signature,
        url: `https://github.com/${i.repo}/releases/download/v${i.version}/${i.installer}`,
      },
    },
  };
  return `${JSON.stringify(manifest, null, 2)}\n`;
}

function arg(args: string[], name: string): string | undefined {
  const at = args.indexOf(`--${name}`);
  return at >= 0 ? args[at + 1] : undefined;
}

function main(args: string[]): void {
  const need = (name: string): string => {
    const v = arg(args, name);
    if (!v) throw new Error(`missing --${name}`);
    return v;
  };
  const out = need("out");
  const json = build({
    version: need("version"),
    installer: need("installer"),
    installerBytes: readFileSync(need("installer-file")),
    signature: readFileSync(need("sig"), "utf8"),
    pubkey: need("pubkey"),
    repo: need("repo"),
    date: arg(args, "date") ?? new Date().toISOString().replace(/\.\d{3}Z$/, "Z"),
  });
  writeFileSync(out, json);
  console.log(`wrote ${out}`);
}

if (import.meta.main) {
  try {
    main(process.argv.slice(2));
  } catch (e) {
    console.error(`latest.json: ${e instanceof Error ? e.message : String(e)}`);
    process.exit(1);
  }
}
