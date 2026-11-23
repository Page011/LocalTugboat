import { describe, expect, test } from "bun:test";
import { readFileSync } from "node:fs";
import { join } from "node:path";
import { UPDATE_CHECK_HELP, UPDATE_CHECK_LABEL, UPDATE_INSTALLING_TEXT, updateBannerText } from "./updates";

const root = join(import.meta.dir, "..", "..");
const read = (...p: string[]) => readFileSync(join(root, ...p), "utf8");

describe("update texts", () => {
  test("the switch and its sub-line say what was promised", () => {
    expect(UPDATE_CHECK_LABEL).toBe("Check for updates");
    expect(UPDATE_CHECK_HELP).toBe("Once a day, from GitHub. Sends nothing about you or your files.");
  });

  test("the banner is one line with the version", () => {
    expect(updateBannerText("0.3.1")).toBe("UPDATE READY - Tugboat 0.3.1");
    expect(updateBannerText("1.0.0-rc.1")).not.toContain("\n");
  });

  test("the installing line is one short line", () => {
    expect(UPDATE_INSTALLING_TEXT).toBe("INSTALLING - Windows will ask for permission");
    expect(UPDATE_INSTALLING_TEXT).not.toContain("\n");
  });

  test("Settings shows the switch with that sub-line, bound to the update commands", () => {
    const svelte = read("src", "lib", "Settings.svelte");
    expect(svelte).toContain("{UPDATE_CHECK_LABEL}");
    expect(svelte).toContain("{UPDATE_CHECK_HELP}");
    expect(svelte).toContain("api.updateCheck()");
    expect(svelte).toContain("api.setUpdateCheck(on)");
    // On until the PC says otherwise: an unanswered switch is shown on (and disabled), never off.
    expect(svelte).toContain("checked={updateCheck ?? true}");
  });

  test("the banner button is wired to the install call", () => {
    const svelte = read("src", "lib", "Banners.svelte");
    expect(svelte).toContain('"update-install": installUpdate');
    expect(read("src", "lib", "app.svelte.ts")).toContain("api.updateInstall()");
    // The banner is told while the call runs, and a second click while it runs does nothing.
    expect(svelte).toContain("installing: app.installing");
    const state = read("src", "lib", "app.svelte.ts");
    expect(state).toContain("if (app.installing) return false;");
    expect(state).toContain("app.installing = false;");
  });
});
