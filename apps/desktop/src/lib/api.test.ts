import { describe, expect, mock, test } from "bun:test";
import { readFileSync } from "node:fs";
import { join } from "node:path";

// The Tauri bridge is replaced by a recorder: the test checks the wrappers, not Tauri.
const calls: Array<{ cmd: string; args: unknown }> = [];
let nextError: unknown = null;
mock.module("@tauri-apps/api/core", () => ({
  invoke: async (cmd: string, args?: unknown) => {
    calls.push({ cmd, args });
    if (nextError !== null) {
      const e = nextError;
      nextError = null;
      throw e;
    }
    return null;
  },
}));
mock.module("@tauri-apps/api/event", () => ({ listen: async () => () => {} }));
mock.module("@tauri-apps/plugin-dialog", () => ({ open: async () => null }));

const { api, ApiError, EVENTS, thumbUrl } = await import("./api");

const root = join(import.meta.dir, "..", "..");
const rust = (f: string) => readFileSync(join(root, "src-tauri", "src", f), "utf8");

describe("api.ts and the Rust commands stay in step", () => {
  const commandsRs = rust("commands.rs").split("#[cfg(test)]")[0]!;
  const rustCommands = [...commandsRs.matchAll(/#\[tauri::command[^\]]*\]\s*pub (?:async )?fn (\w+)/g)].map((m) => m[1]!);
  const apiSrc = readFileSync(join(root, "src", "lib", "api.ts"), "utf8");
  const used = [...apiSrc.matchAll(/call<[^(]*\("(\w+)"/g)].map((m) => m[1]!);

  test("every Rust command has a wrapper and every wrapper has a command", () => {
    expect(rustCommands.length).toBeGreaterThanOrEqual(30);
    expect([...used].sort()).toEqual([...rustCommands].sort());
  });

  test("every command is registered with the Tauri handler", () => {
    const libRs = rust("lib.rs");
    const registered = [...libRs.matchAll(/commands::(\w+),/g)].map((m) => m[1]!);
    expect([...registered].sort()).toEqual([...rustCommands].sort());
  });

  test("event names match the Rust constants", () => {
    const be = rust("backend.rs");
    for (const [key, name] of Object.entries(EVENTS)) {
      expect(be, `event ${key}`).toContain(`"${name}"`);
    }
  });
});

describe("wrappers", () => {
  test("arguments use the names Tauri maps to the Rust parameters (camelCase -> snake_case)", async () => {
    calls.length = 0;
    await api.libraryChoose("D:\\Local Tugboat", true);
    await api.items({ folder: null, bin: false, after: null, limit: 50 });
    await api.pairingConfirm(true);
    expect(calls).toEqual([
      { cmd: "library_choose", args: { path: "D:\\Local Tugboat", acceptCloud: true } },
      { cmd: "items", args: { query: { folder: null, bin: false, after: null, limit: 50 } } },
      { cmd: "pairing_confirm", args: { accept: true } },
    ]);
  });

  test("a CmdError from Rust becomes an ApiError with its kind and one-line message", async () => {
    nextError = { kind: "not_empty", message: "LIBRARY MUST BE EMPTY - D:\\x already has files" };
    const e = await api.libraryChoose("D:\\x", false).catch((x) => x);
    expect(e).toBeInstanceOf(ApiError);
    expect(e.kind).toBe("not_empty");
    expect(e.message).toContain("MUST BE EMPTY");
  });

  test("a rejection that is not a CmdError is still an ApiError, never a bare string", async () => {
    nextError = "ipc exploded";
    const e = await api.syncStatus().catch((x) => x);
    expect(e).toBeInstanceOf(ApiError);
    expect(e.kind).toBe("unknown");
    expect(e.message).toBe("ipc exploded");
  });

  test("thumbnail URLs point at the snapthumb scheme", () => {
    expect(thumbUrl("0197-abc" as never)).toBe("http://snapthumb.localhost/0197-abc");
  });
});
