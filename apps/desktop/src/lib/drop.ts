// Pure text for dragging files onto the window: the overlay, the progress line, the result.
// Rust decides what happened (src-tauri/src/drop.rs); this only words it, one short line each.

import type { DropLine } from "./bindings/DropLine";
import type { DropProgress } from "./bindings/DropProgress";
import type { DropReport } from "./bindings/DropReport";
import type { DropState } from "./bindings/DropState";

export type DropEnter = Extract<DropState, { t: "enter" }>;

/** The overlay's headline: names the real target. A drag of only folders says they will be zipped first. */
export const dropTitle = (s: DropEnter): string => (s.files === 0 && s.folders > 0 ? `Zip and copy to ${s.target_name}` : `Copy to ${s.target_name}`);

/** The overlay's second line: only when files AND a folder are in the drag (each folder becomes one .zip). */
export const dropSub = (s: DropEnter): string | null => (s.files > 0 && s.folders > 0 ? (s.folders === 1 ? "The folder is zipped first" : "Folders are zipped first") : null);

/** Whole percent of the file (or folder) being copied, null when its size is unknown. */
export function dropPercent(p: DropProgress): number | null {
  if (p.total_bytes <= 0) return null;
  return Math.min(100, Math.floor((p.done_bytes / p.total_bytes) * 100));
}

export function dropProgressText(p: DropProgress): string {
  const pct = dropPercent(p);
  return `${p.zip ? "Zipping" : "Copying"} ${p.file}${p.count > 1 ? ` (${p.index} of ${p.count})` : ""}${pct === null ? "" : `, ${pct}%`}`;
}

/** The success line ("3 copied to Inbox"), null when nothing was copied. */
export function dropSummary(r: DropReport): string | null {
  return r.copied.length === 0 ? null : `${r.copied.length} copied to ${r.folder_name}`;
}

/** Everything that was not copied: refused first, then failed. */
export const dropProblems = (r: DropReport): DropLine[] => [...r.refused, ...r.failed];

/** The banner for a drop that left something behind: stays until dismissed. null = nothing to say.
 *  One reason for all: that reason, the names after it. Different reasons: a count, then one line per file with its own reason. */
export function dropBanner(r: DropReport): { tone: "danger" | "warn"; text: string; detail?: string; lines?: string[] } | null {
  const problems = dropProblems(r);
  if (problems.length === 0) return null;
  const tone = r.failed.length > 0 ? "danger" : "warn";
  const reasons = new Set(problems.map((l) => l.reason));
  if (reasons.size === 1) {
    const names = problems.map((l) => l.name).filter((n) => n !== "");
    return { tone, text: problems[0]!.reason, detail: names.length > 0 ? names.join(", ") : undefined };
  }
  return { tone, text: `${problems.length} FILES NOT COPIED`, lines: problems.map((l) => (l.name === "" ? l.reason : `${l.name}: ${l.reason}`)) };
}
