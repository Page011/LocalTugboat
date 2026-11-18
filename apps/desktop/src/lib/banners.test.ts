import { describe, expect, test } from "bun:test";
import { computeBanners, type BannerInput } from "./banners";
import type { LibraryStatus } from "./bindings/LibraryStatus";

const lib = (over: Partial<LibraryStatus> = {}): LibraryStatus => ({
  path: "C:\\Snap", health: { t: "ready" }, message: null, cloud_warning: null, not_synced: [], cannot_mark: false, last_scan_at: null, last_error: null, ...over,
});
const base: BannerInput = {
  info: { version: "0.1.0", device_name: "PC", device_id: "aa", bind: "127.0.0.1:47300", network_error: null, data_dir: "C:\\d" },
  sync: { state: "synced", peer_name: "Pixel", last_synced_at: 1, waiting: 0, downloading: 0, storage_low: false, message: null },
  library: lib(),
  firewall: { status: { t: "ok" }, message: null, can_fix: false },
  error: null,
  drop: null,
};
const ids = (i: BannerInput) => computeBanners(i).map((b) => b.id);

describe("computeBanners", () => {
  test("a healthy app shows nothing", () => {
    expect(computeBanners(base)).toEqual([]);
  });
  test("nothing loaded yet shows nothing", () => {
    expect(computeBanners({ info: null, sync: null, library: null, firewall: null, error: null, drop: null })).toEqual([]);
  });
  test("firewall problem: Fix only when it can help, always a recheck", () => {
    const fw = (can_fix: boolean) => ({ ...base, firewall: { status: { t: "rule_missing" as const }, message: "WINDOWS FIREWALL is blocking Tugboat", can_fix } });
    expect(computeBanners(fw(true))[0]?.actions.map((a) => a.id)).toEqual(["fw-fix", "fw-recheck"]);
    expect(computeBanners(fw(false))[0]?.actions.map((a) => a.id)).toEqual(["fw-recheck"]);
    expect(computeBanners(fw(true))[0]?.tone).toBe("danger");
  });
  test("library missing offers Locate and a recheck; the path is detail, not text", () => {
    const b = computeBanners({ ...base, library: lib({ health: { t: "missing", path: "D:\\Local Tugboat" }, message: "LIBRARY FOLDER NOT FOUND - D:\\Local Tugboat" }) });
    expect(b).toHaveLength(1);
    expect(b[0]).toMatchObject({ text: "LIBRARY FOLDER NOT FOUND", detail: "D:\\Local Tugboat" });
    expect(b[0]?.actions.map((a) => a.id)).toEqual(["lib-locate", "lib-rescan"]);
  });
  test("bulk missing offers exactly the two plan buttons", () => {
    const b = computeBanners({ ...base, library: lib({ health: { t: "bulk_missing", missing: 142, total: 900 } }) });
    expect(b[0]?.text).toBe("142 of 900 files MISSING - sync paused");
    // The core message repeats the buttons; the banner never shows it.
    const withMsg = computeBanners({ ...base, library: lib({ health: { t: "bulk_missing", missing: 142, total: 900 }, message: "142 FILES MISSING - move them to the Bin or keep waiting" }) });
    expect(withMsg[0]?.text).toBe("142 of 900 files MISSING - sync paused");
    expect(b[0]?.actions.map((a) => a.label)).toEqual(["Move them to the Bin", "Keep waiting"]);
  });
  test("cloud folder, scan error and not-synced are warnings", () => {
    const b = computeBanners({
      ...base,
      library: lib({ cloud_warning: "OneDrive", last_error: "locked", not_synced: [{ path: "a.txt", name: "a.txt", reason: "REFUSED - shortcut and folder-setting files never sync" }] }),
    });
    expect(b.map((x) => [x.id, x.tone])).toEqual([["cloud", "warn"], ["scan-error", "warn"], ["not-synced", "warn"]]);
    expect(b[2]?.text).toBe("1 file NOT SYNCED");
  });
  test("a scan error is not repeated while the library itself is the problem", () => {
    expect(ids({ ...base, library: lib({ health: { t: "missing", path: "D:\\x" }, last_error: "gone" }) })).toEqual(["library"]);
  });
  test("version mismatch shows the refusal text", () => {
    const b = computeBanners({ ...base, sync: { ...base.sync!, state: "refused", message: "Update Tugboat on the PC" } });
    expect(b[0]).toMatchObject({ id: "refused", tone: "danger", text: "SYNC REFUSED - Update Tugboat on the PC" });
  });
  test("a refusal is said once: not again as an error banner", () => {
    const sync = { ...base.sync!, state: "refused" as const, message: "Update Tugboat on the phone" };
    expect(ids({ ...base, sync, error: { kind: "refused", message: "Update Tugboat on the phone" } })).toEqual(["refused"]);
    expect(ids({ ...base, sync, error: { kind: "x", message: "ERROR text: Update Tugboat on the phone" } })).toEqual(["refused"]);
    expect(ids({ ...base, error: { kind: "refused", message: "REFUSED - shortcut files never sync" } })).toEqual([]);
    expect(ids({ ...base, sync, error: { kind: "x", message: "disk full" } })).toEqual(["refused", "error"]);
  });
  test("network down and a core error", () => {
    const b = computeBanners({ ...base, info: { ...base.info!, network_error: "port 47300 in use" }, error: { kind: "x", message: "disk full" } });
    expect(b.map((x) => x.id)).toEqual(["network", "error"]);
    expect(b[0]?.text).toBe("NETWORK DOWN - port 47300 in use");
  });
  test("a drive that cannot hold the Mark-of-the-Web is a blocker with the count of waiting files", () => {
    const b = computeBanners({ ...base, library: lib({ cannot_mark: true }), sync: { ...base.sync!, downloading: 4 } });
    expect(b[0]).toMatchObject({ id: "cannot-mark", tone: "danger", text: "LIBRARY DRIVE CANNOT MARK DOWNLOADS - files from the phone wait (4)" });
    expect(computeBanners({ ...base, library: lib({ cannot_mark: true }) })[0]?.text).toBe("LIBRARY DRIVE CANNOT MARK DOWNLOADS - files from the phone wait");
    expect(b[0]?.actions.map((a) => a.id)).toEqual(["lib-choose"]);
  });
  test("a drop that left files behind stays as a banner until dismissed; a clean drop shows none", () => {
    const clean = { folder_name: "Inbox", copied: ["a.pdf"], refused: [], failed: [] };
    expect(computeBanners({ ...base, drop: clean })).toEqual([]);
    const b = computeBanners({ ...base, drop: { ...clean, refused: [{ name: "x.lnk", reason: "REFUSED - shortcut and folder-setting files never sync" }] } });
    expect(b).toHaveLength(1);
    expect(b[0]).toMatchObject({ id: "drop", tone: "warn", text: "REFUSED - shortcut and folder-setting files never sync", detail: "x.lnk" });
    expect(b[0]?.actions.map((a) => a.id)).toEqual(["dismiss-drop"]);
    const failed = computeBanners({ ...base, drop: { ...clean, failed: [{ name: "big.mp4", reason: "COPY FAILED - The device is not ready." }] } });
    expect(failed[0]?.tone).toBe("danger");
  });
  test("blockers come before warnings", () => {
    const b = computeBanners({ ...base, library: lib({ cloud_warning: "OneDrive" }), firewall: { status: { t: "all_inbound_blocked" }, message: null, can_fix: false } });
    expect(b.map((x) => x.id)).toEqual(["firewall", "cloud"]);
  });
  test("a late blocker (refused) still sorts above an earlier warning (cloud)", () => {
    const b = computeBanners({ ...base, library: lib({ cloud_warning: "OneDrive" }), sync: { ...base.sync!, state: "refused", message: null } });
    expect(b.map((x) => x.id)).toEqual(["refused", "cloud"]);
  });
});
