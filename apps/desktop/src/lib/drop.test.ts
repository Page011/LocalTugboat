import { describe, expect, test } from "bun:test";
import { dropBanner, dropPercent, dropProblems, dropProgressText, dropSub, dropSummary, dropTitle, type DropEnter } from "./drop";
import type { DropReport } from "./bindings/DropReport";

const report = (over: Partial<DropReport> = {}): DropReport => ({ folder_name: "Inbox", copied: [], refused: [], failed: [], ...over });
const enter = (over: Partial<DropEnter> = {}): DropEnter => ({ t: "enter", files: 2, folders: 0, target_name: "Inbox", ...over });

describe("the drop overlay", () => {
  test("names the target folder", () => {
    expect(dropTitle(enter())).toBe("Copy to Inbox");
    expect(dropTitle(enter({ target_name: "Work" }))).toBe("Copy to Work");
  });
  test("only folders: the headline says they are zipped, naming the real target", () => {
    expect(dropTitle(enter({ files: 0, folders: 1 }))).toBe("Zip and copy to Inbox");
    expect(dropTitle(enter({ files: 0, folders: 2, target_name: "Work" }))).toBe("Zip and copy to Work");
    expect(dropSub(enter({ files: 0, folders: 2 }))).toBeNull();
  });
  test("a folder next to files: the headline stays a copy, one line says the folder is zipped; files alone get none", () => {
    expect(dropSub(enter())).toBeNull();
    expect(dropTitle(enter({ folders: 1 }))).toBe("Copy to Inbox");
    expect(dropSub(enter({ folders: 1 }))).toBe("The folder is zipped first");
    expect(dropSub(enter({ folders: 3 }))).toBe("Folders are zipped first");
  });
});

describe("progress", () => {
  const p = (done: number, total: number, index = 1, count = 1, zip = false) => ({ file: "big.mp4", index, count, done_bytes: done, total_bytes: total, zip });
  test("percent rounds down, never passes 100 and is null for an unknown size", () => {
    expect(dropPercent(p(0, 100))).toBe(0);
    expect(dropPercent(p(999, 1000))).toBe(99);
    expect(dropPercent(p(1000, 1000))).toBe(100);
    expect(dropPercent(p(2000, 1000))).toBe(100);
    expect(dropPercent(p(5, 0))).toBeNull();
  });
  test("the line names the file, its place in the queue and the percent", () => {
    expect(dropProgressText(p(500, 1000))).toBe("Copying big.mp4, 50%");
    expect(dropProgressText(p(500, 1000, 2, 3))).toBe("Copying big.mp4 (2 of 3), 50%");
    expect(dropProgressText(p(5, 0))).toBe("Copying big.mp4");
  });
  test("a folder being zipped says so", () => {
    expect(dropProgressText({ ...p(500, 1000), file: "Trip", zip: true })).toBe("Zipping Trip, 50%");
    expect(dropProgressText({ ...p(500, 1000, 2, 2, true), file: "Trip" })).toBe("Zipping Trip (2 of 2), 50%");
  });
});

describe("the result", () => {
  test("a success line only when something was copied", () => {
    expect(dropSummary(report({ copied: ["a.pdf", "b.pdf", "c.pdf"] }))).toBe("3 copied to Inbox");
    expect(dropSummary(report({ copied: ["a.pdf"], folder_name: "Work" }))).toBe("1 copied to Work");
    expect(dropSummary(report())).toBeNull();
  });
  test("a clean drop has no banner", () => {
    expect(dropBanner(report({ copied: ["a.pdf"] }))).toBeNull();
  });
  test("one refused file: its own reason as the text, the name as the detail, a warning", () => {
    const b = dropBanner(report({ refused: [{ name: "x.lnk", reason: "REFUSED - shortcut and folder-setting files never sync" }] }));
    expect(b).toEqual({ tone: "warn", text: "REFUSED - shortcut and folder-setting files never sync", detail: "x.lnk" });
  });
  test("the same reason for several files stays one line with all the names", () => {
    const b = dropBanner(report({ refused: [{ name: "a", reason: "FOLDER IS EMPTY - nothing to zip" }, { name: "b", reason: "FOLDER IS EMPTY - nothing to zip" }] }));
    expect(b).toMatchObject({ text: "FOLDER IS EMPTY - nothing to zip", detail: "a, b" });
  });
  test("a folder that could not be zipped is one red line with the file and the reason, no separate name", () => {
    const b = dropBanner(report({ failed: [{ name: "", reason: "ZIP FAILED - Trip: IMG_1.jpg The process cannot access the file because it is being used by another process." }] }));
    expect(b).toEqual({ tone: "danger", text: "ZIP FAILED - Trip: IMG_1.jpg The process cannot access the file because it is being used by another process.", detail: undefined });
  });
  test("a zip that was made counts as a copied item in the success line", () => {
    expect(dropSummary(report({ copied: ["Trip.zip"] }))).toBe("1 copied to Inbox");
  });
  test("different reasons are counted with one line per file and its own reason; any failure makes it red", () => {
    const b = dropBanner(report({ refused: [{ name: "a.lnk", reason: "REFUSED" }], failed: [{ name: "big.mp4", reason: "COPY FAILED - The device is not ready." }] }));
    expect(b).toEqual({ tone: "danger", text: "2 FILES NOT COPIED", lines: ["a.lnk: REFUSED", "big.mp4: COPY FAILED - The device is not ready."] });
  });
  test("a whole-drop line is the headline and the files refused before it are the lines, never counted as files", () => {
    // 10 files dropped, x.lnk refused, then the drive is full: the count must not say "2 FILES".
    const b = dropBanner(report({ refused: [{ name: "x.lnk", reason: "REFUSED" }, { name: "", reason: "NOT ENOUGH SPACE on D: - nothing copied" }] }));
    expect(b).toEqual({ tone: "warn", text: "NOT ENOUGH SPACE on D: - nothing copied", lines: ["x.lnk: REFUSED"] });
  });
  test("a folder that failed to zip counts as an item, a whole-drop line does not", () => {
    const b = dropBanner(report({ refused: [{ name: "a.lnk", reason: "REFUSED" }], failed: [{ name: "", reason: "ZIP FAILED - Trip: IMG_1.jpg locked" }] }));
    expect(b).toEqual({ tone: "danger", text: "2 ITEMS NOT COPIED", lines: ["a.lnk: REFUSED", "ZIP FAILED - Trip: IMG_1.jpg locked"] });
  });
  test("a whole-drop line has no name and no detail", () => {
    const b = dropBanner(report({ refused: [{ name: "", reason: "PAUSED - nothing copied" }] }));
    expect(b).toEqual({ tone: "warn", text: "PAUSED - nothing copied", detail: undefined });
    expect(dropProblems(report({ refused: [{ name: "", reason: "x" }], failed: [{ name: "f", reason: "y" }] })).map((l) => l.reason)).toEqual(["x", "y"]);
  });
});
