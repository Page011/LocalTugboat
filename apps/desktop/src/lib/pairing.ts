// Pairing screen state machine, pure so the rules are tested: which core event moves the
// screen where. The store feeds it the begin result, core pairing events, and the QR timer.

import type { PairingEvent } from "./bindings/PairingEvent";
import type { PairingStart } from "./bindings/PairingStart";

export type PairPhase =
  | { t: "idle" }
  /** QR on screen until start.expires_at. */
  | { t: "qr"; start: PairingStart }
  /** The phone dialled in: "Phone shows 482 913? [Pair] [Cancel]". */
  | { t: "code"; code: string; peerName: string }
  | { t: "done"; peerName: string }
  | { t: "cancelled"; message: string };

export type PairInput =
  | { t: "begun"; start: PairingStart }
  | { t: "event"; e: PairingEvent }
  /** The user pressed Cancel or dismissed a result. */
  | { t: "reset" }
  /** The QR timer ran out. */
  | { t: "expired" };

/** "482913" becomes "482 913" (the form the phone shows). */
export const groupCode = (code: string): string => (code.length === 6 ? `${code.slice(0, 3)} ${code.slice(3)}` : code);

export function nextPairPhase(s: PairPhase, i: PairInput): PairPhase {
  switch (i.t) {
    case "begun":
      return { t: "qr", start: i.start };
    case "reset":
      return { t: "idle" };
    case "expired":
      // A code already on screen is waiting for the owner's click, not for the clock.
      return s.t === "qr" ? { t: "cancelled", message: "QR EXPIRED - make a new one" } : s;
    case "event": {
      const e = i.e;
      // Logged out (here or by the phone): a "Linked to X" or half-finished screen would be stale.
      if (e.kind === "unpaired" && (s.t === "done" || s.t === "qr" || s.t === "code")) return { t: "idle" };
      // Events while no pairing is open on this screen belong to nobody here: ignore them.
      if (s.t === "idle" || s.t === "done" || s.t === "cancelled") return s;
      switch (e.kind) {
        case "code":
          return { t: "code", code: e.code, peerName: e.peer_name };
        case "completed":
          return { t: "done", peerName: e.peer_name };
        case "cancelled":
          return { t: "cancelled", message: e.message };
        case "window_open":
        case "unpaired":
          return s;
      }
    }
  }
}
