import { describe, expect, test } from "bun:test";
import { actionLabel, ago, binLabel, deviceLabel, duration, fileIcon, fmtSize, isProblem, itemLabel, itemMeta, kindText, lastSeenText, parseBinDays, peerNote, shortPath, syncLabel } from "./format";
import type { ItemView } from "./bindings/ItemView";
import type { SyncStatus } from "./bindings/SyncStatus";

const NOW = 1_800_000_000_000;
const MIN = 60_000;
const HOUR = 60 * MIN;
const DAY = 24 * HOUR;

function item(over: Partial<ItemView> = {}): ItemView {
  return {
    id: "i1", name: "IMG_1.jpg", folder: "f1", kind: "image", size: 1000, ext: "jpg", captured_at: NOW, origin: "d1",
    pinned: false, state: { t: "live", expires_at: NOW + 5 * HOUR + 10 * MIN }, local: "present", path: "C:\\x.jpg", thumb_path: null, never_reached_peer: false,
    ...over,
  };
}

describe("duration", () => {
  test("rounds down to the largest useful unit", () => {
    expect(duration(0)).toBe("<1m");
    expect(duration(59_999)).toBe("<1m");
    expect(duration(MIN)).toBe("1m");
    expect(duration(59 * MIN + 59_000)).toBe("59m");
    expect(duration(HOUR)).toBe("1h");
    expect(duration(23 * HOUR + 59 * MIN)).toBe("23h");
    expect(duration(DAY)).toBe("1d");
    expect(duration(DAY + 12 * HOUR)).toBe("1d 12h");
    expect(duration(3 * DAY + 12 * HOUR)).toBe("3d");
    expect(duration(7 * DAY)).toBe("7d");
  });
});

describe("itemLabel", () => {
  test("a live item counts down", () => {
    expect(itemLabel(item(), NOW)).toEqual({ text: "Expires in 5h", tone: "neutral", icon: "clock" });
  });
  test("under an hour is a warning", () => {
    const l = itemLabel(item({ state: { t: "live", expires_at: NOW + 20 * MIN } }), NOW);
    expect(l.text).toBe("Expires in 20m");
    expect(l.tone).toBe("warn");
  });
  test("an item past its time but not yet swept says so, never a negative time", () => {
    expect(itemLabel(item({ state: { t: "live", expires_at: NOW - 1 } }), NOW).text).toBe("Expiring now");
  });
  test("kept and folder keep labels (an item no longer waits for the phone: it expires on time)", () => {
    expect(itemLabel(item({ state: { t: "kept" }, pinned: true }), NOW).text).toBe("Kept");
    expect(itemLabel(item({ state: { t: "folder_keep" } }), NOW).text).toBe("Folder keeps");
  });
  test("a missing, refused or downloading file outranks the expiry state", () => {
    expect(itemLabel(item({ local: "lost" }), NOW)).toEqual({ text: "FILE MISSING", tone: "danger", icon: "alert" });
    expect(itemLabel(item({ local: "refused" }), NOW).text).toBe("REFUSED");
    expect(itemLabel(item({ local: "pending" }), NOW).text).toBe("Downloading");
  });
});

describe("binLabel", () => {
  const binned = (t: "deleted" | "expired", at: number) => item({ state: { t, binned_at: at }, local: "binned" });
  test("shows why and how long is left of the Bin days", () => {
    expect(binLabel(binned("deleted", NOW - DAY), 7, NOW).text).toBe("Deleted, 6d left");
    expect(binLabel(binned("expired", NOW - 6 * DAY - 12 * HOUR), 7, NOW)).toEqual({ text: "Expired, 12h left", tone: "warn", icon: "trash" });
  });
  test("at or past the limit this PC is about to remove the file", () => {
    expect(binLabel(binned("deleted", NOW - 7 * DAY), 7, NOW).text).toBe("Deleted, removing now");
  });
  test("the Bin days setting changes the count", () => {
    expect(binLabel(binned("deleted", NOW), 30, NOW).text).toBe("Deleted, 30d left");
  });
});

describe("syncLabel", () => {
  const s = (over: Partial<SyncStatus>): SyncStatus => ({ state: "synced", peer_name: "Pixel", last_synced_at: NOW - 2 * MIN, waiting: 0, downloading: 0, storage_low: false, message: null, ...over });
  test("synced shows the last synced time", () => {
    expect(syncLabel(s({}), NOW)).toEqual({ text: "Synced with Pixel, 2 min ago", tone: "ok", icon: "check" });
  });
  test("offline is routine: neutral, sentence case, with the last synced time", () => {
    expect(syncLabel(s({ state: "offline" }), NOW)).toEqual({ text: "Offline - Pixel last synced 2 min ago", tone: "neutral", icon: "wifi-off" });
  });
  test("offline with photos waiting is a warning, still not red", () => {
    const l = syncLabel(s({ state: "offline", waiting: 3 }), NOW);
    expect(l.tone).toBe("warn");
    expect(l.text).toBe("Offline - Pixel last synced 2 min ago, 3 waiting");
  });
  test("paused shows only the key phrase of the message, never a path", () => {
    expect(syncLabel(s({ state: "paused", message: "LIBRARY FOLDER NOT FOUND - C:\\Users\\x\\Local Tugboat" }), NOW)).toEqual({ text: "PAUSED - LIBRARY FOLDER NOT FOUND", tone: "danger", icon: "pause" });
    expect(syncLabel(s({ state: "paused", message: null }), NOW).text).toBe("PAUSED");
  });
  test("a refused handshake shows the reason with an uppercase key word", () => {
    expect(syncLabel(s({ state: "refused", message: "Update Tugboat on the PC" }), NOW).text).toBe("REFUSED - Update Tugboat on the PC");
  });
  test("never synced does not read 'last synced never'", () => {
    expect(syncLabel(s({ state: "offline", last_synced_at: null, peer_name: null }), NOW).text).toBe("Offline - phone never synced");
  });
});

describe("small helpers", () => {
  test("ago", () => {
    expect(ago(null, NOW)).toBe("never");
    expect(ago(NOW - 5_000, NOW)).toBe("just now");
    expect(ago(NOW + 5_000, NOW)).toBe("just now");
    expect(ago(NOW - 90 * MIN, NOW)).toBe("1 h ago");
    expect(ago(NOW - 3 * DAY, NOW)).toBe("3 d ago");
  });
  test("fmtSize", () => {
    expect(fmtSize(500)).toBe("500 B");
    expect(fmtSize(1536)).toBe("1.5 KB");
    expect(fmtSize(5 * 1024 * 1024)).toBe("5.0 MB");
    expect(fmtSize(250 * 1024 * 1024)).toBe("250 MB");
  });
  test("problem actions", () => {
    expect(isProblem("conflict")).toBe(true);
    expect(isProblem("moved")).toBe(false);
  });
  test("every problem action has an uppercase key word", () => {
    for (const a of ["refused", "conflict", "lost", "paused", "sync_error"] as const) expect(actionLabel(a)).toBe(actionLabel(a).toUpperCase());
  });
  test("shortPath keeps the drive and the last folders", () => {
    expect(shortPath("C:\\a\\b")).toBe("C:\\a\\b");
    expect(shortPath("C:\\Users\\alex\\Documents\\Pictures\\Local Tugboat", 30)).toBe("C:\\...\\Pictures\\Local Tugboat");
    expect(shortPath("C:\\Users\\alex\\Documents\\A-very-long-last-folder-name-here", 30)).toBe("C:\\...\\A-very-long-last-folder-name-here");
  });
});

describe("file tiles", () => {
  test("the icon follows the extension, in any case, and falls back to a plain file", () => {
    expect(fileIcon("pdf")).toBe("text");
    expect(fileIcon("DOCX")).toBe("text");
    expect(fileIcon("xlsx")).toBe("sheet");
    expect(fileIcon("zip")).toBe("archive");
    expect(fileIcon("mp3")).toBe("audio");
    expect(fileIcon("json")).toBe("code");
    expect(fileIcon("svg")).toBe("image");
    expect(fileIcon("avi")).toBe("video");
    expect(fileIcon("")).toBe("file");
    expect(fileIcon("weird")).toBe("file");
    expect(fileIcon("constructor")).toBe("file"); // a name from a synced record is never looked up as an object member
  });
  test("the meta line says Photo, Video, the extension, or File", () => {
    expect(itemMeta(item({ size: 1024 * 1024 + 200 * 1024 }))).toBe("Photo, 1.2 MB");
    expect(itemMeta(item({ kind: "video", ext: "mp4", size: 5 * 1024 * 1024 }))).toBe("Video, 5.0 MB");
    expect(itemMeta(item({ kind: "file", ext: "pdf", size: 1024 * 1024 + 200 * 1024 }))).toBe("PDF, 1.2 MB");
    expect(itemMeta(item({ kind: "file", ext: "", size: 3 }))).toBe("File, 3 B");
    expect(kindText({ kind: "file", ext: "docx" })).toBe("DOCX");
  });
  test("an expired item the phone never confirmed says NOT ON PHONE", () => {
    expect(peerNote(item())).toBeNull();
    expect(peerNote(item({ never_reached_peer: true }))).toBe("NOT ON PHONE");
  });
  test("a file item is labelled like any other item", () => {
    expect(itemLabel(item({ kind: "file", ext: "pdf" }), NOW).text).toBe("Expires in 5h");
  });
});

describe("last seen", () => {
  test("connected while a session is open, else the time", () => {
    const paired = { last_seen_at: NOW - 3 * HOUR };
    expect(lastSeenText(paired, { state: "synced" }, NOW)).toBe("Connected now");
    expect(lastSeenText(paired, { state: "syncing" }, NOW)).toBe("Connected now");
    expect(lastSeenText(paired, { state: "offline" }, NOW)).toBe("Last seen 3 h ago");
    expect(lastSeenText(paired, { state: "connecting" }, NOW)).toBe("Last seen 3 h ago");
    expect(lastSeenText(paired, null, NOW)).toBe("Last seen 3 h ago");
    expect(lastSeenText({ last_seen_at: null }, { state: "offline" }, NOW)).toBe("Last seen never");
  });
});

describe("deviceLabel", () => {
  const me = "aa" as never;
  const phone = { id: "bb" as never, name: "Sam's phone" };
  test("system, this PC, the paired phone and a former phone", () => {
    expect(deviceLabel(null, me, phone)).toBe("System");
    expect(deviceLabel(me, me, phone)).toBe("This PC");
    expect(deviceLabel(phone.id, me, phone)).toBe("Sam's phone");
    expect(deviceLabel("cc" as never, me, phone)).toBe("Earlier phone");
    expect(deviceLabel("cc" as never, me, null)).toBe("Earlier phone");
  });
});

describe("parseBinDays", () => {
  test("accepts 1 to 365", () => {
    expect(parseBinDays("7")).toEqual({ ok: true, days: 7 });
    expect(parseBinDays(" 365 ")).toEqual({ ok: true, days: 365 });
    expect(parseBinDays("1")).toEqual({ ok: true, days: 1 });
  });
  test("rejects zero, too big, decimals, signs, empty and text", () => {
    for (const bad of ["0", "366", "2.5", "-3", "", "abc", "1e2"]) expect(parseBinDays(bad).ok, bad).toBe(false);
  });
});
