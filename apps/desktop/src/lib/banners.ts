// Which banners show right now, in priority order. Pure: the App component renders the list and
// maps each action id to a call. One short line each, uppercase key word for blockers.

import type { AppInfo } from "./bindings/AppInfo";
import type { DropReport } from "./bindings/DropReport";
import type { FirewallInfo } from "./bindings/FirewallInfo";
import type { LibraryStatus } from "./bindings/LibraryStatus";
import type { SyncStatus } from "./bindings/SyncStatus";
import { dropBanner } from "./drop";

export type BannerAction = "fw-fix" | "fw-recheck" | "lib-locate" | "lib-rescan" | "lib-bin" | "lib-wait" | "lib-choose" | "not-synced" | "dismiss-error" | "dismiss-drop";

export interface Banner {
  id: string;
  /** danger = blocker (red), warn = needs a look. */
  tone: "danger" | "warn";
  text: string;
  /** Long detail (a path) shown shortened on the same line, in full as a tooltip. */
  detail?: string;
  /** The detail is plain text (file names), not a path: normal font, no shortening. */
  plain?: boolean;
  /** One short line per item under the banner text (a drop with different reasons per file). */
  lines?: string[];
  actions: Array<{ id: BannerAction; label: string }>;
}

export interface BannerInput {
  info: AppInfo | null;
  sync: SyncStatus | null;
  library: LibraryStatus | null;
  firewall: FirewallInfo | null;
  /** The last core "error" event, until dismissed. */
  error: { kind: string; message: string } | null;
  /** The last drop that left files behind (refused or failed), until dismissed. */
  drop: DropReport | null;
}

export function computeBanners(i: BannerInput): Banner[] {
  const out: Banner[] = [];
  if (i.info?.network_error) {
    out.push({ id: "network", tone: "danger", text: `NETWORK DOWN - ${i.info.network_error}`, actions: [] });
  }
  if (i.firewall && i.firewall.status.t !== "ok") {
    const actions: Banner["actions"] = [];
    if (i.firewall.can_fix) actions.push({ id: "fw-fix", label: "Fix" });
    actions.push({ id: "fw-recheck", label: "Check again" });
    out.push({ id: "firewall", tone: "danger", text: i.firewall.message ?? "WINDOWS FIREWALL may be blocking Tugboat", actions });
  }
  const lib = i.library;
  if (lib) {
    const h = lib.health;
    if (h.t === "not_configured") {
      out.push({ id: "library", tone: "danger", text: lib.message ?? "LIBRARY NOT CHOSEN - pick a folder", actions: [{ id: "lib-choose", label: "Choose folder" }] });
    } else if (h.t === "missing") {
      // The core message is "KEY PHRASE - path"; the path goes in `detail`, so the banner stays one line.
      out.push({
        id: "library",
        tone: "danger",
        text: lib.message?.split(" - ")[0] ?? "LIBRARY FOLDER NOT FOUND",
        detail: h.path,
        actions: [{ id: "lib-locate", label: "Locate" }, { id: "lib-rescan", label: "Check again" }],
      });
    } else if (h.t === "bulk_missing") {
      out.push({
        id: "library",
        tone: "danger",
        text: `${h.missing} of ${h.total} files MISSING - sync paused`,
        actions: [{ id: "lib-bin", label: "Move them to the Bin" }, { id: "lib-wait", label: "Keep waiting" }],
      });
    }
    if (lib.cannot_mark) {
      // Received files are marked as downloaded; this drive cannot hold the mark, so they wait here instead of being saved unmarked.
      const n = i.sync?.downloading ?? 0;
      out.push({ id: "cannot-mark", tone: "danger", text: `LIBRARY DRIVE CANNOT MARK DOWNLOADS - files from the phone wait${n > 0 ? ` (${n})` : ""}`, actions: [{ id: "lib-choose", label: "Choose another" }] });
    }
    if (lib.cloud_warning) {
      out.push({ id: "cloud", tone: "warn", text: `CLOUD FOLDER - ${lib.cloud_warning}`, actions: [{ id: "lib-choose", label: "Choose another" }] });
    }
    if (lib.last_error && h.t === "ready") {
      out.push({ id: "scan-error", tone: "warn", text: `SCAN ERROR - ${lib.last_error}`, actions: [{ id: "lib-rescan", label: "Scan again" }] });
    }
    if (lib.not_synced.length > 0) {
      const n = lib.not_synced.length;
      out.push({ id: "not-synced", tone: "warn", text: `${n} ${n === 1 ? "file" : "files"} NOT SYNCED`, actions: [{ id: "not-synced", label: "View" }] });
    }
  }
  if (i.sync?.state === "refused") {
    out.push({ id: "refused", tone: "danger", text: `SYNC REFUSED${i.sync.message ? ` - ${i.sync.message}` : ""}`, actions: [] });
  }
  const drop = i.drop ? dropBanner(i.drop) : null;
  if (drop) {
    out.push({ id: "drop", tone: drop.tone, text: drop.text, detail: drop.detail, plain: true, lines: drop.lines, actions: [{ id: "dismiss-drop", label: "Dismiss" }] });
  }
  // The same words twice (the core's error event repeats what the SYNC REFUSED banner already says): show once.
  const refusedText = i.sync?.state === "refused" ? i.sync.message : null;
  // A refused file already has its own Not synced row and banner: its error event adds nothing.
  if (i.error && i.error.kind !== "refused" && !(refusedText && i.error.message.includes(refusedText))) {
    out.push({ id: "error", tone: "danger", text: `ERROR - ${i.error.message}`, actions: [{ id: "dismiss-error", label: "Dismiss" }] });
  }
  // Blockers first; the sort is stable, so each group keeps the order above.
  return out.sort((a, b) => Number(a.tone !== "danger") - Number(b.tone !== "danger"));
}
