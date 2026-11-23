// The one place the webview talks to Rust. Every Tauri command and event has a typed wrapper
// here; the types are generated from the Rust types (src/lib/bindings, kept in step by the
// drift test in src-tauri/src/bindings.rs). Screens import from here and nowhere else.
//
// Errors: every command rejects with a CmdError ({kind, message}); the wrappers throw it as an
// ApiError so a screen can show `message` (one short line) and branch on `kind`.

import { invoke } from "@tauri-apps/api/core";
import { listen, type UnlistenFn } from "@tauri-apps/api/event";
import { open } from "@tauri-apps/plugin-dialog";

import type { ActivityEntry } from "./bindings/ActivityEntry";
import type { AppInfo } from "./bindings/AppInfo";
import type { BinCount } from "./bindings/BinCount";
import type { CmdError } from "./bindings/CmdError";
import type { DropProgress } from "./bindings/DropProgress";
import type { DropReport } from "./bindings/DropReport";
import type { DropState } from "./bindings/DropState";
import type { Event as CoreEvent } from "./bindings/Event";
import type { FirewallInfo } from "./bindings/FirewallInfo";
import type { FolderId } from "./bindings/FolderId";
import type { FolderView } from "./bindings/FolderView";
import type { ItemId } from "./bindings/ItemId";
import type { ItemQuery } from "./bindings/ItemQuery";
import type { ItemView } from "./bindings/ItemView";
import type { LibraryStatus } from "./bindings/LibraryStatus";
import type { Page } from "./bindings/Page";
import type { PairedDevice } from "./bindings/PairedDevice";
import type { PairingStart } from "./bindings/PairingStart";
import type { Settings } from "./bindings/Settings";
import type { SyncStatus } from "./bindings/SyncStatus";
import type { TrayState } from "./bindings/TrayState";

export type {
  ActivityEntry, AppInfo, CmdError, CoreEvent, DropProgress, DropReport, DropState, FirewallInfo, FolderId, FolderView, ItemId, ItemQuery,
  ItemView, LibraryStatus, Page, PairedDevice, PairingStart, Settings, SyncStatus, TrayState,
};

export class ApiError extends Error {
  readonly kind: string;
  constructor(e: CmdError) {
    super(e.message);
    this.name = "ApiError";
    this.kind = e.kind;
  }
}

function isCmdError(e: unknown): e is CmdError {
  return typeof e === "object" && e !== null && typeof (e as CmdError).kind === "string" && typeof (e as CmdError).message === "string";
}

/** Run a command; a rejected call becomes an ApiError whatever its shape. */
async function call<T>(cmd: string, args?: Record<string, unknown>): Promise<T> {
  try {
    return await invoke<T>(cmd, args);
  } catch (e) {
    throw new ApiError(isCmdError(e) ? e : { kind: "unknown", message: typeof e === "string" ? e : String(e) });
  }
}

export const api = {
  // app and status
  appInfo: () => call<AppInfo>("app_info"),
  syncStatus: () => call<SyncStatus>("sync_status"),
  quit: () => call<void>("quit_app"),

  // gallery and Bin
  folders: () => call<FolderView[]>("folders"),
  items: (query: ItemQuery) => call<Page<ItemView>>("items", { query }),
  setPinned: (id: ItemId, pinned: boolean) => call<void>("item_set_pinned", { id, pinned }),
  moveItem: (id: ItemId, folder: FolderId) => call<void>("item_move", { id, folder }),
  renameItem: (id: ItemId, name: string) => call<void>("item_rename", { id, name }),
  deleteItem: (id: ItemId) => call<void>("item_delete", { id }),
  restoreItem: (id: ItemId) => call<void>("item_restore", { id }),
  /** Delete forever: the item leaves the Bin here and on every linked device. */
  purgeItem: (id: ItemId) => call<void>("item_purge", { id }),
  /** Empty Bin: Delete forever for every item the Bin lists; `shown` and `token` are what `binCount` gave the question,
   *  and a Bin that lists other items now (even the same number) deletes nothing. Resolves to how many. */
  emptyBin: (shown: number, token: string) => call<number>("bin_empty", { shown, token }),
  /** How many items Empty Bin would delete, and the token that names exactly those items. */
  binCount: () => call<BinCount>("bin_count"),
  openItem: (id: ItemId) => call<void>("item_open", { id }),
  revealItem: (id: ItemId) => call<void>("item_reveal", { id }),

  // activity
  activity: (before: number | null, limit: number) => call<ActivityEntry[]>("activity", { before, limit }),

  // settings
  settings: () => call<Settings>("settings_get"),
  saveSettings: (settings: Settings) => call<void>("settings_save", { settings }),
  autostart: () => call<boolean>("autostart_get"),
  setAutostart: (enabled: boolean) => call<void>("autostart_set", { enabled }),

  // pairing
  pairedDevice: () => call<PairedDevice | null>("paired_device"),
  pairingBegin: () => call<PairingStart>("pairing_begin"),
  pairingConfirm: (accept: boolean) => call<void>("pairing_confirm", { accept }),
  pairingCancel: () => call<void>("pairing_cancel"),
  unpair: () => call<void>("unpair"),

  // library folder
  libraryStatus: () => call<LibraryStatus>("library_status"),
  libraryChoose: (path: string, acceptCloud: boolean) => call<void>("library_choose", { path, acceptCloud }),
  libraryLocate: (path: string) => call<void>("library_locate", { path }),
  /** [Move them to the Bin] = true, [Keep waiting] = false. */
  libraryResolveMissing: (bin: boolean) => call<void>("library_resolve_missing", { bin }),
  libraryRescan: () => call<void>("library_rescan"),
  libraryOpenFolder: () => call<void>("library_open_folder"),

  /** The gallery's current folder (null = All = Inbox): where a file dropped on the window lands. A folder id, never a path. */
  dropTargetSet: (folder: FolderId | null) => call<void>("drop_target_set", { folder }),

  // Windows Firewall
  firewallStatus: () => call<FirewallInfo>("firewall_status"),
  firewallRecheck: () => call<FirewallInfo>("firewall_recheck"),
  firewallFix: () => call<void>("firewall_fix"),
};

// ---------- events ----------

/** Event names; src-tauri/src/backend.rs `ev` has the same list. */
export const EVENTS = {
  core: "core-event",
  library: "library-status",
  firewall: "firewall-status",
  resync: "resync",
  tray: "tray-state",
  dropState: "drop-state",
  dropProgress: "drop-progress",
  dropResult: "drop-result",
} as const;

/** Core events: sync status, items/folders changed, pairing steps, errors. */
export const onCoreEvent = (cb: (e: CoreEvent) => void): Promise<UnlistenFn> => listen<CoreEvent>(EVENTS.core, (m) => cb(m.payload));
export const onLibraryStatus = (cb: (s: LibraryStatus) => void): Promise<UnlistenFn> => listen<LibraryStatus>(EVENTS.library, (m) => cb(m.payload));
export const onFirewallStatus = (cb: (s: FirewallInfo) => void): Promise<UnlistenFn> => listen<FirewallInfo>(EVENTS.firewall, (m) => cb(m.payload));
export const onTrayState = (cb: (s: TrayState) => void): Promise<UnlistenFn> => listen<TrayState>(EVENTS.tray, (m) => cb(m.payload));
/** Files dragged over the window (the overlay) and what a drop did. Rust sends them from the window's native drag-drop event. */
export const onDropState = (cb: (s: DropState) => void): Promise<UnlistenFn> => listen<DropState>(EVENTS.dropState, (m) => cb(m.payload));
export const onDropProgress = (cb: (p: DropProgress) => void): Promise<UnlistenFn> => listen<DropProgress>(EVENTS.dropProgress, (m) => cb(m.payload));
export const onDropResult = (cb: (r: DropReport) => void): Promise<UnlistenFn> => listen<DropReport>(EVENTS.dropResult, (m) => cb(m.payload));
/** The event stream lagged: re-read everything the screen shows. */
export const onResync = (cb: () => void): Promise<UnlistenFn> => listen(EVENTS.resync, () => cb());

// ---------- files ----------

/** URL of an item's thumbnail (served by the `snapthumb` scheme; 404 when it has none). */
export const thumbUrl = (id: ItemId): string => `http://snapthumb.localhost/${id}`;

/** What a tile's <img> loads: null when the item has no thumbnail yet (show the icon, make no request).
 *  The query changes when the thumbnail path does, so a tile that was mounted before the thumbnail existed
 *  asks again; the scheme handler reads only the path and ignores the query. */
export const thumbSrc = (item: { id: ItemId; thumb_path: string | null }): string | null =>
  item.thumb_path === null ? null : `${thumbUrl(item.id)}?v=${encodeURIComponent(item.thumb_path)}`;

/** Folder picker for the library location. null = cancelled. */
export async function pickFolder(): Promise<string | null> {
  const p = await open({ directory: true, multiple: false });
  return typeof p === "string" ? p : null;
}
