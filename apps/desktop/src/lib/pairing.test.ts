import { describe, expect, test } from "bun:test";
import { groupCode, nextPairPhase, type PairPhase } from "./pairing";

const start = { svg: "<svg/>", payload: "p", expires_at: 1 };
const qr: PairPhase = { t: "qr", start };
const code: PairPhase = { t: "code", code: "482913", peerName: "Pixel" };

describe("nextPairPhase", () => {
  test("begin shows the QR", () => {
    expect(nextPairPhase({ t: "idle" }, { t: "begun", start })).toEqual(qr);
  });
  test("a phone dialling in shows its code for the owner to confirm", () => {
    expect(nextPairPhase(qr, { t: "event", e: { kind: "code", code: "482913", peer_name: "Pixel" } })).toEqual(code);
  });
  test("completion and cancellation end the flow with their text", () => {
    expect(nextPairPhase(code, { t: "event", e: { kind: "completed", peer: "d", peer_name: "Pixel" } })).toEqual({ t: "done", peerName: "Pixel" });
    const msg = "PAIRING BLOCKED - someone else used this code";
    expect(nextPairPhase(qr, { t: "event", e: { kind: "cancelled", code: "secret_reused", message: msg } })).toEqual({ t: "cancelled", message: msg });
  });
  test("a second phone using the code cancels even while the first code is on screen", () => {
    expect(nextPairPhase(code, { t: "event", e: { kind: "cancelled", code: "secret_reused", message: "x" } }).t).toBe("cancelled");
  });
  test("QR timer ends only a waiting QR, never a code awaiting the owner click", () => {
    expect(nextPairPhase(qr, { t: "expired" })).toEqual({ t: "cancelled", message: "QR EXPIRED - make a new one" });
    expect(nextPairPhase(code, { t: "expired" })).toBe(code);
  });
  test("events with no pairing open are ignored", () => {
    const idle: PairPhase = { t: "idle" };
    expect(nextPairPhase(idle, { t: "event", e: { kind: "code", code: "123456", peer_name: "x" } })).toBe(idle);
    const done: PairPhase = { t: "done", peerName: "x" };
    expect(nextPairPhase(done, { t: "event", e: { kind: "cancelled", code: "c", message: "m" } })).toBe(done);
  });
  test("a log out ends a Linked-to screen, a QR and a code", () => {
    const unpaired = { t: "event" as const, e: { kind: "unpaired" as const, peer: "d", by_peer: true } };
    expect(nextPairPhase({ t: "done", peerName: "x" }, unpaired)).toEqual({ t: "idle" });
    expect(nextPairPhase(qr, unpaired)).toEqual({ t: "idle" });
    expect(nextPairPhase(code, unpaired)).toEqual({ t: "idle" });
    // completed, then the old phone's log out arrives
    const done = nextPairPhase(code, { t: "event", e: { kind: "completed", peer: "d", peer_name: "Pixel" } });
    expect(nextPairPhase(done, { t: "event", e: { kind: "unpaired", peer: "o", by_peer: false } })).toEqual({ t: "idle" });
  });
  test("a log out with nothing open, or an error screen, changes nothing", () => {
    const idle: PairPhase = { t: "idle" };
    expect(nextPairPhase(idle, { t: "event", e: { kind: "unpaired", peer: "d", by_peer: true } })).toBe(idle);
    const cancelled: PairPhase = { t: "cancelled", message: "m" };
    expect(nextPairPhase(cancelled, { t: "event", e: { kind: "unpaired", peer: "d", by_peer: true } })).toBe(cancelled);
  });
  test("reset goes back to idle", () => {
    expect(nextPairPhase({ t: "done", peerName: "x" }, { t: "reset" })).toEqual({ t: "idle" });
  });
  test("groupCode", () => {
    expect(groupCode("482913")).toBe("482 913");
    expect(groupCode("12")).toBe("12");
  });
});
