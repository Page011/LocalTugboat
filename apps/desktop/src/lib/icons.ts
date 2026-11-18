// IconKey (from format.ts) to lucide component. Monochrome only: icons take the text colour.
import { Check, CircleAlert, Clock, Download, Hourglass, Infinity as InfinityIcon, Pause, Pin, RefreshCw, Trash2, WifiOff } from "@lucide/svelte";
import type { Component } from "svelte";
import type { IconKey } from "./format";

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
