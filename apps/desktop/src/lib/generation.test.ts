import { describe, expect, test } from "bun:test";
import { claimAll, lastWins, reportRun, startRun } from "./generation";

describe("claimAll", () => {
  test("an untouched claim checks true for every field", () => {
    const gen = { a: 0, b: 0 };
    const ok = claimAll(gen);
    expect(ok("a") && ok("b")).toBe(true);
  });
  test("a later claim invalidates the earlier one (overlapping reloads)", () => {
    const gen = { a: 0, b: 0 };
    const first = claimAll(gen);
    const second = claimAll(gen);
    expect(first("a")).toBe(false);
    expect(first("b")).toBe(false);
    expect(second("a") && second("b")).toBe(true);
  });
  test("an event or field loader bumping one counter fails only that field", () => {
    const gen = { a: 0, b: 0 };
    const ok = claimAll(gen);
    gen.a++;
    expect(ok("a")).toBe(false);
    expect(ok("b")).toBe(true);
  });
});

describe("startRun", () => {
  test("only the newest run is the latest", () => {
    const c = { n: 0 };
    const a = startRun(c);
    expect(a()).toBe(true);
    const b = startRun(c);
    expect(a()).toBe(false);
    expect(b()).toBe(true);
  });
});

describe("reportRun (the decision reload() applies)", () => {
  const fresh = () => ({ ready: false, loadError: null as string | null });
  const apply = (s: { ready: boolean; loadError: string | null }, n: { ready: boolean; loadError: string | null }) => {
    s.ready = n.ready;
    s.loadError = n.loadError;
  };
  test("an older reload finishing after a newer failed one cannot declare ready", () => {
    const c = { n: 0 };
    const s = fresh();
    const a = startRun(c);
    const b = startRun(c);
    apply(s, reportRun(b, { ok: false, error: "boom" }, s));
    apply(s, reportRun(a, { ok: true }, s));
    expect(s).toEqual({ ready: false, loadError: "boom" });
  });
  test("an older reload failing after a newer success cannot show an error", () => {
    const c = { n: 0 };
    const s = fresh();
    const a = startRun(c);
    const b = startRun(c);
    apply(s, reportRun(b, { ok: true }, s));
    apply(s, reportRun(a, { ok: false, error: "late" }, s));
    expect(s).toEqual({ ready: true, loadError: null });
  });
  test("the newest success clears an earlier error; the newest failure keeps ready", () => {
    const c = { n: 0 };
    const s = { ready: true, loadError: "old" as string | null };
    expect(reportRun(startRun(c), { ok: true }, s)).toEqual({ ready: true, loadError: null });
    expect(reportRun(startRun(c), { ok: false, error: "x" }, { ready: true, loadError: null })).toEqual({ ready: true, loadError: "x" });
  });
});

describe("lastWins", () => {
  /** A saver whose requests finish when the test says so. */
  function rig(saved: number) {
    const gates: Array<(ok: boolean) => void> = [];
    const st = { saved, calls: [] as number[], finish: (i: number, ok: boolean) => gates[i]?.(ok) };
    const submit = lastWins<number>(
      () => st.saved,
      (v) => new Promise<boolean>((res) => {
        st.calls.push(v);
        gates.push((ok) => {
          if (ok) st.saved = v;
          res(ok);
        });
      }),
    );
    return { st, submit };
  }
  const tick = () => new Promise((r) => setTimeout(r, 0));

  test("saved 7, submit 30 then 7 before the first save ends: 7 is what ends up saved", async () => {
    const { st, submit } = rig(7);
    const p1 = submit(30);
    await tick();
    expect(st.calls).toEqual([30]); // the save is in flight
    const p2 = submit(7);
    st.finish(0, true);
    await tick();
    expect(st.calls).toEqual([30, 7]);
    st.finish(1, true);
    await Promise.all([p1, p2]);
    expect(st.saved).toBe(7);
  });
  test("only the last of several pending submits is sent", async () => {
    const { st, submit } = rig(7);
    const all = [submit(30)];
    await tick();
    all.push(submit(31), submit(32), submit(33)); // the first save is in flight
    st.finish(0, true);
    await tick();
    expect(st.calls).toEqual([30, 33]);
    st.finish(1, true);
    await Promise.all(all);
    expect(st.saved).toBe(33);
  });
  test("an unchanged value with nothing in flight is skipped", async () => {
    const { st, submit } = rig(7);
    await submit(7);
    expect(st.calls).toEqual([]);
  });
  test("a failed first save does not stop the last value; it is skipped when it equals what is saved", async () => {
    const { st, submit } = rig(7);
    const p = [submit(30)];
    await tick();
    p.push(submit(7)); // the first save is in flight
    st.finish(0, false);
    await Promise.all(p);
    expect(st.calls).toEqual([30]);
    expect(st.saved).toBe(7);
  });
  test("submits in one task coalesce to the last one", async () => {
    const { st, submit } = rig(7);
    const p = [submit(30), submit(31), submit(7)];
    await Promise.all(p);
    expect(st.calls).toEqual([]);
  });
  test("two submits in one task, the first equal to what is saved: the second is still saved", async () => {
    const { st, submit } = rig(7);
    const p = [submit(7), submit(30)];
    await tick();
    expect(st.calls).toEqual([30]);
    st.finish(0, true);
    await Promise.all(p);
    expect(st.saved).toBe(30);
  });
  test("a submit after the run ended starts a new one", async () => {
    const { st, submit } = rig(7);
    const p = submit(30);
    await tick();
    st.finish(0, true);
    await p;
    const q = submit(31);
    await tick();
    expect(st.calls).toEqual([30, 31]);
    st.finish(1, true);
    await q;
    expect(st.saved).toBe(31);
  });
});
