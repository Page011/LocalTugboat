// Shared UI state: what Rust last told us (sync, library, firewall, folders, paired phone,
// settings) kept current from events, plus the pairing flow. Screens read `app` and call the
// functions here; only this file and api.ts talk to Rust.
//
// Events carry ids, not rows, so a screen that shows rows watches `app.itemsRev` / `activityRev`
// and re-reads. If the event stream lags (`resync`) everything is re-read.

import { api, ApiError, onCoreEvent, onDropProgress, onDropResult, onDropState, onFirewallStatus, onLibraryStatus, onResync, pickFolder } from "./api";
import type { AppInfo, DropProgress, DropReport, FirewallInfo, FolderView, LibraryStatus, PairedDevice, Settings, SyncStatus } from "./api";
import { dropBanner, dropSummary, type DropEnter } from "./drop";
import { claimAll, reportRun, startRun, type RunOutcome } from "./generation";
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
  /** The phone logged this PC out: a banner until dismissed or a phone is linked again. */
  removedByPhone: false,
  /** Last failure (a rejected command or a core error event), until dismissed. */
  error: null as { kind: string; message: string } | null,
  /** The chosen library folder looks cloud-synced: ask before using it. */
  cloudPrompt: null as { path: string; message: string } | null,
  notSyncedOpen: false,
  itemsRev: 0,
  activityRev: 0,
  /** Files dragged onto the window: the overlay (`over`), the copy in progress and the success line. */
  drop: {
    over: null as DropEnter | null,
    progress: null as DropProgress | null,
    /** Progress shows only for a copy that runs longer than 300 ms. */
    showProgress: false,
    note: null as { text: string; until: number } | null,
  },
  /** What the last drop left behind (refused or failed files): a banner until dismissed. */
  dropReport: null as DropReport | null,
});

const PROGRESS_AFTER_MS = 300;
const NOTE_FOR_MS = 5000;
let progressTimer: ReturnType<typeof setTimeout> | undefined;

function dropFinished(r: DropReport): void {
  clearTimeout(progressTimer);
  progressTimer = undefined;
  // `over` is not touched: the drop's own Leave hid the overlay, and a drag started since may be showing it.
  app.drop.progress = null;
  app.drop.showProgress = false;
  const text = dropSummary(r);
  app.drop.note = text === null ? null : { text, until: Date.now() + NOTE_FOR_MS };
  // A refused or failed file is never silent: the banner stays until the user dismisses it.
  app.dropReport = dropBanner(r) === null ? app.dropReport : r;
}

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

/** One counter per field the events and the loaders both write. A loader bumps its counter before the request and
 *  applies the answer only if nobody wrote the field since: a slow read must not undo a newer event. A full reload
 *  claims every counter (claimAll), so an older overlapping reload cannot overwrite a newer one. Plain (not
 *  reactive) on purpose. */
const gen = { sync: 0, library: 0, firewall: 0, folders: 0, paired: 0, settings: 0 };

function pair(i: PairInput): void {
  app.pairing = nextPairPhase(app.pairing, i);
}

async function loadFolders(): Promise<void> {
  const g = ++gen.folders;
  const folders = await api.folders();
  if (g === gen.folders) app.folders = folders;
}
async function loadPaired(): Promise<void> {
  const g = ++gen.paired;
  const paired = await api.pairedDevice();
  if (g === gen.paired) app.paired = paired;
}

/** Read everything. Applies every answer that came back, then throws the first failure so the caller can show
 *  the load error. (The claim above voids answers of loaders already in flight, so one failed request must not
 *  also drop the other fields' fresh answers.) */
async function loadAll(): Promise<void> {
  const current = claimAll(gen);
  const all = await Promise.allSettled([
    api.appInfo(), api.syncStatus(), api.libraryStatus(), api.firewallStatus(), api.folders(), api.pairedDevice(), api.settings(),
  ] as const);
  const [info, sync, library, firewall, folders, paired, settings] = all;
  if (info.status === "fulfilled") app.info = info.value;
  // A field an event wrote while these requests ran is newer than what came back: keep it.
  if (sync.status === "fulfilled" && current("sync")) app.sync = sync.value;
  if (library.status === "fulfilled" && current("library")) app.library = library.value;
  if (firewall.status === "fulfilled" && current("firewall")) app.firewall = firewall.value;
  if (folders.status === "fulfilled" && current("folders")) app.folders = folders.value;
  if (paired.status === "fulfilled" && current("paired")) app.paired = paired.value;
  if (settings.status === "fulfilled" && current("settings")) app.settings = settings.value;
  app.itemsRev++;
  app.activityRev++;
  const failed = all.find((r) => r.status === "rejected");
  if (failed) throw failed.reason;
}

/** Counts reloads: only the newest one reports `ready` / `loadError`. An older reload's fields were voided by the
 *  newer claim, so it must not declare success while the newer one failed (a field would stay null). */
const reloads = { n: 0 };

export async function reload(): Promise<void> {
  const latest = startRun(reloads);
  let outcome: RunOutcome;
  try {
    await loadAll();
    outcome = { ok: true };
  } catch (e) {
    outcome = { ok: false, error: e instanceof Error ? e.message : String(e) };
  }
  const next = reportRun(latest, outcome, app);
  app.ready = next.ready;
  app.loadError = next.loadError;
}

/** Connect to Rust: first load, events, and the 1 s clock (labels and the QR countdown). */
export async function start(): Promise<() => void> {
  const stops: Array<() => void> = [];
  const refreshFolders = () => void loadFolders().catch(fail);
  const refreshPaired = () => void loadPaired().catch(fail);
  // Each disposer is kept as soon as its listener exists: a later listen() that rejects must not leave the
  // earlier ones registered.
  try {
    stops.push(await onCoreEvent((e) => {
      switch (e.t) {
        case "sync": {
          const { t: _t, ...s } = e;
          const changed = app.sync?.state !== s.state;
          gen.sync++;
          app.sync = s;
          // The core writes "last seen" at the first hello and when a session ends: re-read it on every state change.
          if (changed) refreshPaired();
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
        case "settings_changed": {
          const g = ++gen.settings;
          void api.settings().then((s) => { if (g === gen.settings) app.settings = s; }).catch(fail);
          break;
        }
        case "pairing":
          pair({ t: "event", e });
          if (e.kind === "unpaired") app.removedByPhone = e.by_peer;
          if (e.kind === "completed") app.removedByPhone = false;
          if (e.kind === "completed" || e.kind === "unpaired") refreshPaired();
          break;
        case "error":
          app.error = { kind: e.kind, message: e.message };
          break;
        case "places_changed":
          break;
      }
    }));
    stops.push(await onLibraryStatus((s) => { gen.library++; app.library = s; }));
    stops.push(await onDropState((s) => (app.drop.over = s.t === "enter" ? s : null)));
    stops.push(await onDropProgress((p) => {
      if (app.drop.progress === null) progressTimer = setTimeout(() => (app.drop.showProgress = true), PROGRESS_AFTER_MS);
      app.drop.progress = p;
    }));
    stops.push(await onDropResult(dropFinished));
    stops.push(await onFirewallStatus((s) => { gen.firewall++; app.firewall = s; }));
    stops.push(await onResync(() => void reload()));
  } catch (e) {
    stops.forEach((s) => s());
    throw e;
  }
  await reload();
  const timer = setInterval(() => {
    app.now = Date.now();
    if (app.drop.note !== null && app.now >= app.drop.note.until) app.drop.note = null;
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
    const g = ++gen.library;
    const library = await api.libraryStatus();
    if (g === gen.library) app.library = library;
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
