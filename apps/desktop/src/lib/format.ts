// Pure text helpers: every label the screens show comes from here, so the rules (when an
// item says "Expires in 5h", what a file tile says about its type) are tested without a window.
// Times are unix ms; `now` is always passed in so a test controls the clock.

import type { ActivityAction } from "./bindings/ActivityAction";
import type { DeviceId } from "./bindings/DeviceId";
import type { ItemView } from "./bindings/ItemView";
import type { Retention } from "./bindings/Retention";
import type { SyncStatus } from "./bindings/SyncStatus";

export type Tone = "neutral" | "ok" | "warn" | "danger";
/** Which lucide icon a label gets; the screens map the key to a component. */
export type IconKey = "clock" | "hourglass" | "pin" | "infinity" | "trash" | "alert" | "download" | "check" | "wifi-off" | "refresh" | "pause";

export interface Label {
  text: string;
  tone: Tone;
  icon: IconKey;
}

const MIN = 60_000;
const HOUR = 60 * MIN;
const DAY = 24 * HOUR;

/** Time left as a short phrase: "<1m", "42m", "5h", "1d 12h", "4d". Never negative. */
export function duration(ms: number): string {
  if (ms < MIN) return "<1m";
  if (ms < HOUR) return `${Math.floor(ms / MIN)}m`;
  if (ms < DAY) return `${Math.floor(ms / HOUR)}h`;
  const d = Math.floor(ms / DAY);
  const h = Math.floor((ms % DAY) / HOUR);
  return d < 3 && h > 0 ? `${d}d ${h}h` : `${d}d`;
}

/** Label of an item in the gallery (not the Bin). */
export function itemLabel(it: ItemView, now: number): Label {
  // A missing or refused file outranks every expiry state: the user must see the problem.
  if (it.local === "lost") return { text: "FILE MISSING", tone: "danger", icon: "alert" };
  if (it.local === "refused") return { text: "REFUSED", tone: "danger", icon: "alert" };
  if (it.local === "pending") return { text: "Downloading", tone: "neutral", icon: "download" };
  const s = it.state;
  switch (s.t) {
    case "kept":
      return { text: "Kept", tone: "ok", icon: "pin" };
    case "folder_keep":
      return { text: "Folder keeps", tone: "ok", icon: "infinity" };
    case "live":
      return s.expires_at <= now
        ? { text: "Expiring now", tone: "warn", icon: "clock" }
        : { text: `Expires in ${duration(s.expires_at - now)}`, tone: s.expires_at - now < HOUR ? "warn" : "neutral", icon: "clock" };
    case "deleted":
      return { text: "Deleted", tone: "neutral", icon: "trash" };
    case "expired":
      return { text: "Expired", tone: "neutral", icon: "trash" };
  }
}

/** Which icon a file tile gets (lucide, monochrome); the tile maps the key to a component. */
export type FileIconKey = "file" | "text" | "sheet" | "archive" | "audio" | "code" | "image" | "video";

const FILE_ICON_BY_EXT: Record<string, FileIconKey> = {
  pdf: "text", doc: "text", docx: "text", odt: "text", rtf: "text", txt: "text", md: "text", log: "text",
  xls: "sheet", xlsx: "sheet", ods: "sheet", csv: "sheet", tsv: "sheet",
  zip: "archive", "7z": "archive", rar: "archive", tar: "archive", gz: "archive", tgz: "archive", bz2: "archive", xz: "archive",
  mp3: "audio", wav: "audio", flac: "audio", ogg: "audio", m4a: "audio", aac: "audio", opus: "audio",
  js: "code", ts: "code", json: "code", html: "code", css: "code", xml: "code", py: "code", rs: "code", sh: "code", bat: "code", ps1: "code", sql: "code", yml: "code", yaml: "code",
  svg: "image", psd: "image", ai: "image", raw: "image", dng: "image", bmp: "image", tif: "image", tiff: "image",
  avi: "video", mkv: "video", wmv: "video", flv: "video",
};

/** The icon for a file's extension (any case); anything unknown, and no extension, is a plain file. */
export function fileIcon(ext: string): FileIconKey {
  const e = ext.toLowerCase();
  return Object.hasOwn(FILE_ICON_BY_EXT, e) ? FILE_ICON_BY_EXT[e]! : "file";
}

/** What kind of thing an item is, in one word: "Photo", "Video", "PDF" (the extension), or "File" when it has none. */
export function kindText(it: Pick<ItemView, "kind" | "ext">): string {
  if (it.kind === "image") return "Photo";
  if (it.kind === "video") return "Video";
  return it.ext === "" ? "File" : it.ext.toUpperCase();
}

/** The line under a tile's name: "PDF, 1.2 MB". */
export const itemMeta = (it: Pick<ItemView, "kind" | "ext" | "size">): string => `${kindText(it)}, ${fmtSize(it.size)}`;

/** Said in the Bin when the item expired and the phone never confirmed holding it (it may exist on this PC only). */
export const peerNote = (it: Pick<ItemView, "never_reached_peer">): string | null => (it.never_reached_peer ? "NOT ON PHONE" : null);

/** Label of an item in the Bin: why it is there and when this PC removes its file. */
export function binLabel(it: ItemView, binDays: number, now: number): Label {
  const s = it.state;
  if (s.t !== "deleted" && s.t !== "expired") return itemLabel(it, now);
  const why = s.t === "deleted" ? "Deleted" : "Expired";
  const left = s.binned_at + binDays * DAY - now;
  return left <= 0
    ? { text: `${why}, removing now`, tone: "warn", icon: "trash" }
    : { text: `${why}, ${duration(left)} left`, tone: left < DAY ? "warn" : "neutral", icon: "trash" };
}

/** "just now", "5 min ago", "3 h ago", "2 d ago", then the date. */
export function ago(at: number | null, now: number): string {
  if (at === null) return "never";
  const d = Math.max(0, now - at);
  if (d < MIN) return "just now";
  if (d < HOUR) return `${Math.floor(d / MIN)} min ago`;
  if (d < DAY) return `${Math.floor(d / HOUR)} h ago`;
  if (d < 30 * DAY) return `${Math.floor(d / DAY)} d ago`;
  return new Date(at).toLocaleDateString();
}

/** A Windows path shortened to one line: the drive, "...", then as many of the last folders as fit. */
export function shortPath(path: string, max = 44): string {
  const parts = path.split("\\");
  if (path.length <= max || parts.length <= 3) return path;
  for (const keep of [2, 1]) {
    const tail = parts.slice(-keep).join("\\");
    const out = parts[0] + "\\...\\" + tail;
    if (out.length <= max || keep === 1) return out;
  }
  return path;
}

export function fmtSize(bytes: number): string {
  if (bytes < 1024) return `${bytes} B`;
  const units = ["KB", "MB", "GB", "TB"];
  let v = bytes / 1024;
  let i = 0;
  while (v >= 1024 && i < units.length - 1) {
    v /= 1024;
    i++;
  }
  return `${v >= 100 ? v.toFixed(0) : v.toFixed(1)} ${units[i]}`;
}

/** The paired phone's contact line: "Connected now" while a session is open, else when it was last seen. */
export function lastSeenText(paired: { last_seen_at: number | null }, sync: Pick<SyncStatus, "state"> | null, now: number): string {
  return sync && (sync.state === "syncing" || sync.state === "synced") ? "Connected now" : `Last seen ${ago(paired.last_seen_at, now)}`;
}

/** Local date and time for the Activity list. */
export function fmtTime(at: number): string {
  const d = new Date(at);
  const p = (n: number) => String(n).padStart(2, "0");
  return `${d.getFullYear()}-${p(d.getMonth() + 1)}-${p(d.getDate())} ${p(d.getHours())}:${p(d.getMinutes())}:${p(d.getSeconds())}`;
}

export function retentionLabel(r: Retention): string {
  return r.t === "keep" ? "Keep" : duration(r.secs * 1000);
}

/** The status bar and the sync dot: state, text, and an icon so colour is never the only cue. */
export function syncLabel(s: SyncStatus, now: number): Label {
  const peer = s.peer_name ?? "phone";
  const waiting = s.waiting > 0 ? `, ${s.waiting} waiting` : "";
  switch (s.state) {
    case "unpaired":
      return { text: "Not linked to a phone", tone: "neutral", icon: "wifi-off" };
    case "offline":
      // The phone away from the Wi-Fi is the normal state for most of the day, so it is not red:
      // red stays for the states that need the user (paused, refused). Warn only when photos wait.
      return { text: `Offline - ${peer} ${s.last_synced_at === null ? "never synced" : `last synced ${ago(s.last_synced_at, now)}`}${waiting}`, tone: s.waiting > 0 ? "warn" : "neutral", icon: "wifi-off" };
    case "connecting":
      return { text: `Connecting to ${peer}`, tone: "neutral", icon: "refresh" };
    case "syncing":
      return { text: `Syncing with ${peer}${s.downloading > 0 ? `, ${s.downloading} downloading` : ""}${waiting}`, tone: "neutral", icon: "refresh" };
    case "synced":
      if (s.waiting > 0) return { text: `Synced with ${peer}, ${s.waiting} waiting`, tone: "warn", icon: "hourglass" };
      // A file still travelling through the phone from another PC: never look finished.
      if (s.downloading > 0) return { text: `Synced with ${peer}, ${s.downloading} on the way`, tone: "neutral", icon: "refresh" };
      return { text: `Synced with ${peer}, ${ago(s.last_synced_at, now)}`, tone: "ok", icon: "check" };
    case "paused":
      // Only the key phrase: the banner above carries the detail (a path would not fit here).
      return { text: `PAUSED${s.message ? ` - ${s.message.split(" - ")[0]}` : ""}`, tone: "danger", icon: "pause" };
    case "refused":
      return { text: `REFUSED${s.message ? ` - ${s.message}` : ""}`, tone: "danger", icon: "alert" };
  }
}

const ACTIONS: Record<ActivityAction, string> = {
  captured: "Captured",
  added: "Added",
  received: "Received",
  moved: "Moved",
  renamed: "Renamed",
  kept: "Kept",
  unkept: "Keep removed",
  edited: "Edited",
  deleted: "Deleted",
  binned: "Moved to Bin",
  expired: "Expired",
  restored: "Restored",
  returned: "Returned",
  purged: "Removed from Bin",
  refused: "REFUSED",
  conflict: "CONFLICT",
  lost: "LOST",
  folder_created: "Folder created",
  folder_renamed: "Folder renamed",
  folder_deleted: "Folder deleted",
  retention_changed: "Retention changed",
  paired: "Linked",
  unpaired: "Logged out",
  paused: "PAUSED",
  resumed: "Resumed",
  sync_error: "SYNC ERROR",
  upgraded: "Upgraded",
};

export const actionLabel = (a: ActivityAction): string => ACTIONS[a];

/** Actions that mean something went wrong: the Activity list marks them with an alert icon. */
export const isProblem = (a: ActivityAction): boolean => a === "refused" || a === "conflict" || a === "lost" || a === "paused" || a === "sync_error";

/** Who an Activity row came from: this PC, the paired phone, or a phone that is no longer paired. */
export function deviceLabel(device: DeviceId | null, selfId: DeviceId | null, paired: { id: DeviceId; name: string } | null): string {
  if (device === null) return "System";
  if (device === selfId) return "This PC";
  if (paired && device === paired.id) return paired.name;
  return "Another device";
}

/** The core words a log out by the phone as "by the other device"; the PC says who. */
export const activityDetail = (a: ActivityAction, detail: string): string => (a === "unpaired" && detail === "by the other device" ? "removed by phone" : detail);

/** Bin days field: a whole number 1..365 (the same range Core enforces, so the error shows before the save). */
export function parseBinDays(text: string): { ok: true; days: number } | { ok: false; message: string } {
  const t = text.trim();
  if (!/^\d+$/.test(t)) return { ok: false, message: "ENTER A WHOLE NUMBER from 1 to 365" };
  const days = Number(t);
  return days >= 1 && days <= 365 ? { ok: true, days } : { ok: false, message: "ENTER A NUMBER from 1 to 365" };
}
