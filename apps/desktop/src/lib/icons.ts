// IconKey (from format.ts) to lucide component. Monochrome only: icons take the text colour.
import {
  Check,
  CircleAlert,
  Clock,
  Download,
  File as FileIcon,
  FileArchive,
  FileCode,
  FileImage,
  FileMusic,
  FilePlay,
  FileSpreadsheet,
  FileText,
  Hourglass,
  Infinity as InfinityIcon,
  Pause,
  Pin,
  RefreshCw,
  Trash2,
  WifiOff,
} from "@lucide/svelte";
import type { Component } from "svelte";
import type { FileIconKey, IconKey } from "./format";

/** Icon for a file tile (see `fileIcon` in format.ts). */
export const fileIcons: Record<FileIconKey, Component<{ size?: number; "aria-hidden"?: boolean | "true" | "false" }>> = {
  file: FileIcon,
  text: FileText,
  sheet: FileSpreadsheet,
  archive: FileArchive,
  audio: FileMusic,
  code: FileCode,
  image: FileImage,
  video: FilePlay,
};

export const icons: Record<IconKey, Component<{ size?: number; "aria-hidden"?: boolean | "true" | "false" }>> = {
  clock: Clock,
  hourglass: Hourglass,
  pin: Pin,
  infinity: InfinityIcon,
  trash: Trash2,
  alert: CircleAlert,
  download: Download,
  check: Check,
  "wifi-off": WifiOff,
  refresh: RefreshCw,
  pause: Pause,
};
