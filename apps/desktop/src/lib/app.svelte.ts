// Shared UI state: what Rust last told us (sync, library, firewall, folders, paired phone,
// settings) kept current from events, plus the pairing flow. Screens read `app` and call the
// functions here; only this file and api.ts talk to Rust.
//
// Events carry ids, not rows, so a screen that shows rows watches `app.itemsRev` / `activityRev`
// and re-reads. If the event stream lags (`resync`) everything is re-read.

import { api, ApiError, onCoreEvent, onFirewallStatus, onLibraryStatus, onResync, pickFolder } from "./api";
import type { AppInfo, FirewallInfo, FolderView, LibraryStatus, PairedDevice, Settings, SyncStatus } from "./api";
import { nextPairPhase, type PairInput, type PairPhase } from "./pairing";

export type Screen = "gallery" | "bin" | "activity" | "pair" | "settings";

export const app = $state({
  ready: false,
  /** The first load failed: nothing on screen can be trusted. */
  loadError: null as string | null,
  screen: "gallery" as Screen,
  now: Date.now(),
  info: null as AppInfo | null,
  sync: null as SyncStatus | null,
  library: null as LibraryStatus | null,
  firewall: null as FirewallInfo | null,
  folders: [] as FolderView[],
  paired: null as PairedDevice | null,
  settings: null as Settings | null,
  pairing: { t: "idle" } as PairPhase,
  /** Last failure (a rejected command or a core error event), until dismissed. */
  error: null as { kind: string; message: string } | null,
  /** The chosen library folder looks cloud-synced: ask before using it. */
  cloudPrompt: null as { path: string; message: string } | null,
  notSyncedOpen: false,
  itemsRev: 0,
  activityRev: 0,
});

/** Show a failure as the red banner. Errors are never swallowed. */
export function fail(e: unknown): void {
  app.error = e instanceof ApiError ? { kind: e.kind, message: e.message } : { kind: "unknown", message: e instanceof Error ? e.message : String(e) };
}

/** Run a UI action; a rejection becomes the error banner. */
export async function act(run: () => Promise<unknown>): Promise<boolean> {
  try {
    await run();
    return true;
  } catch (e) {
    fail(e);
    return false;
  }
}

function pair(i: PairInput): void {
  app.pairing = nextPairPhase(app.pairing, i);
}

async function loadFolders(): Promise<void> {
  app.folders = await api.folders();
}
async function loadPaired(): Promise<void> {
  app.paired = await api.pairedDevice();
}

/** Read everything. Throws on the first failure so the caller can show the load error. */
async function loadAll(): Promise<void> {
  const [info, sync, library, firewall, folders, paired, settings] = await Promise.all([
    api.appInfo(), api.syncStatus(), api.libraryStatus(), api.firewallStatus(), api.folders(), api.pairedDevice(), api.settings(),
  ]);
  Object.assign(app, { info, sync, library, firewall, folders, paired, settings });
  app.itemsRev++;
  app.activityRev++;
}

export async function reload(): Promise<void> {
  try {
    await loadAll();
    app.loadError = null;
    app.ready = true;
  } catch (e) {
    app.loadError = e instanceof Error ? e.message : String(e);
  }
}

/** Connect to Rust: first load, events, and the 1 s clock (labels and the QR countdown). */
export async function start(): Promise<() => void> {
  const stops: Array<() => void> = [];
  const refreshFolders = () => void loadFolders().catch(fail);
  const refreshPaired = () => void loadPaired().catch(fail);
  stops.push(
    await onCoreEvent((e) => {
      switch (e.t) {
        case "sync": {
          const { t: _t, ...s } = e;
          app.sync = s;
          break;
        }
        case "items_changed":
          app.itemsRev++;
          refreshFolders(); // counts changed too
          break;
        case "folders_changed":
          app.itemsRev++;
          refreshFolders();
          break;
        case "activity_added":
          app.activityRev++;
          break;
        case "settings_changed":
          void api.settings().then((s) => (app.settings = s)).catch(fail);
          break;
        case "pairing":
          pair({ t: "event", e });
          if (e.kind === "completed" || e.kind === "unpaired") refreshPaired();
          break;
        case "error":
          app.error = { kind: e.kind, message: e.message };
          break;
        case "places_changed":
          break;
      }
    }),
    await onLibraryStatus((s) => (app.library = s)),
    await onFirewallStatus((s) => (app.firewall = s)),
    await onResync(() => void reload()),
  );
  await reload();
  const timer = setInterval(() => {
    app.now = Date.now();
    const p = app.pairing;
    // The QR stops working at expires_at (PC clock = this clock): close the window now rather
    // than leave a dead code on screen.
    if (p.t === "qr" && app.now >= p.start.expires_at) {
      pair({ t: "expired" });
      void api.pairingCancel().catch(fail);
    }
  }, 1000);
  return () => {
    clearInterval(timer);
    stops.forEach((s) => s());
  };
}

// ---------- pairing ----------

export async function beginPairing(): Promise<void> {
  try {
    pair({ t: "begun", start: await api.pairingBegin() });
    app.screen = "pair";
  } catch (e) {
    fail(e);
  }
}

export async function confirmPairing(accept: boolean): Promise<void> {
  if (!(await act(() => api.pairingConfirm(accept)))) return;
  // Accepting ends in a "completed" event; declining is over now.
  if (!accept) pair({ t: "reset" });
}

export async function cancelPairing(): Promise<void> {
  pair({ t: "reset" });
  await act(() => api.pairingCancel());
}

export function dismissPairing(): void {
  pair({ t: "reset" });
}

// ---------- library folder ----------

/** [Choose folder]: pick, then ask Rust. A cloud-synced folder asks first; any other refusal shows as an error. */
export async function chooseLibrary(): Promise<void> {
  try {
    const path = await pickFolder();
    if (path === null) return;
    await tryChoose(path, false);
  } catch (e) {
    fail(e);
  }
}

async function tryChoose(path: string, acceptCloud: boolean): Promise<void> {
  try {
    await api.libraryChoose(path, acceptCloud);
    app.cloudPrompt = null;
    app.library = await api.libraryStatus();
  } catch (e) {
    if (e instanceof ApiError && e.kind === "cloud_folder") app.cloudPrompt = { path, message: e.message };
    else throw e;
  }
}

export const useCloudFolderAnyway = (): Promise<boolean> => act(async () => app.cloudPrompt && (await tryChoose(app.cloudPrompt.path, true)));

export async function locateLibrary(): Promise<void> {
  try {
    const path = await pickFolder();
    if (path !== null) await api.libraryLocate(path);
  } catch (e) {
    fail(e);
  }
}

export const rescanLibrary = (): Promise<boolean> => act(() => api.libraryRescan());
export const resolveMissing = (bin: boolean): Promise<boolean> => act(() => api.libraryResolveMissing(bin));
