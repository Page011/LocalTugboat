// QA driver for a running dev app. The app must be started with
//   WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS=--remote-debugging-port=9340
//   TUGBOAT_PROFILE=<temp dir>  TUGBOAT_BIND=127.0.0.1
// (see notes/qa-desktop.md). Refuses to touch an app that is not on a QA profile.
//
//   bun scripts/qa.mjs info                 title, visible text, buttons
//   bun scripts/qa.mjs theme dark|light     emulate the system colour scheme (only while this call runs)
//   bun scripts/qa.mjs shot <out dir> <name>   native screenshots <name>-light.png and <name>-dark.png
//   bun scripts/qa.mjs size <width> <height>   resize the window (outer size, px) to check a layout
//   bun scripts/qa.mjs click "<button text or aria-label>"   exact text match
//   bun scripts/qa.mjs eval <file.js>       run the file body (return a value)
//   bun scripts/qa.mjs value "<css selector>"   .value / .textContent of the element

import { execFileSync } from "node:child_process";
import { readFile } from "node:fs/promises";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

const port = Number(process.env.TUGBOAT_QA_PORT ?? 9340);
const [mode, arg, arg2] = process.argv.slice(2);

const pages = await fetch(`http://127.0.0.1:${port}/json/list`).then((r) => r.json());
const page = pages.find((p) => p.type === "page" && /localhost:1430|tauri\.localhost/.test(p.url));
if (!page) throw new Error("Tugboat WebView not found");
const ws = new WebSocket(page.webSocketDebuggerUrl);
let next = 0;
const pending = new Map();
ws.onmessage = (ev) => {
  const m = JSON.parse(ev.data);
  const w = m.id && pending.get(m.id);
  if (w) {
    pending.delete(m.id);
    clearTimeout(w.timer);
    m.error ? w.reject(new Error(JSON.stringify(m.error))) : w.resolve(m.result);
  }
};
await new Promise((res, rej) => {
  ws.onopen = res;
  ws.onerror = rej;
});
const send = (method, params = {}) =>
  new Promise((resolve, reject) => {
    const id = ++next;
    const timer = setTimeout(() => {
      pending.delete(id);
      reject(new Error(`CDP timeout: ${method}`));
    }, 15000);
    pending.set(id, { resolve, reject, timer });
    ws.send(JSON.stringify({ id, method, params }));
  });
async function evaluate(expression) {
  const r = await send("Runtime.evaluate", { expression, awaitPromise: true, returnByValue: true, userGesture: true });
  if (r.exceptionDetails) throw new Error(JSON.stringify(r.exceptionDetails));
  return r.result.value;
}

try {
  // A real app on the real profile must never be driven by a test script.
  const info = await evaluate('window.__TAURI_INTERNALS__.invoke("app_info")');
  if (!/qa-profile/.test(info?.data_dir ?? "")) throw new Error(`Refusing: not a QA profile (${info?.data_dir})`);

  if (mode === "theme") {
    await send("Emulation.setEmulatedMedia", { features: [{ name: "prefers-color-scheme", value: arg === "dark" ? "dark" : "light" }] });
    console.log(`theme ${arg}`);
  } else if (mode === "shot") {
    // The emulation lasts only as long as this debugging session, so the screenshot is taken
    // from inside it, one theme at a time.
    const ps1 = join(dirname(fileURLToPath(import.meta.url)), "screenshot.ps1");
    for (const theme of ["light", "dark"]) {
      await send("Emulation.setEmulatedMedia", { features: [{ name: "prefers-color-scheme", value: theme }] });
      await new Promise((r) => setTimeout(r, 700));
      console.log(execFileSync("powershell", ["-NoProfile", "-ExecutionPolicy", "Bypass", "-File", ps1, "-Title", "Tugboat", "-Out", join(arg, `${arg2}-${theme}.png`)]).toString().trim());
    }
  } else if (mode === "size") {
    // Browser.setWindowBounds does nothing in WebView2, so this is Win32 (scripts/resize.ps1).
    const ps1 = join(dirname(fileURLToPath(import.meta.url)), "resize.ps1");
    console.log(execFileSync("powershell", ["-NoProfile", "-ExecutionPolicy", "Bypass", "-File", ps1, "-Title", "Tugboat", "-Width", arg, "-Height", arg2]).toString().trim());
  } else if (mode === "click") {
    const hit = await evaluate(`(() => {
      const want = ${JSON.stringify(arg)};
      const els = [...document.querySelectorAll("button, summary, [role=option]")];
      const el = els.find((e) => (e.getAttribute("aria-label") ?? e.innerText).trim() === want) ?? els.find((e) => e.innerText.trim().startsWith(want));
      if (!el) return false;
      el.click();
      return true;
    })()`);
    if (!hit) throw new Error(`no button "${arg}"`);
    console.log(`clicked ${arg}`);
  } else if (mode === "eval") {
    console.log(JSON.stringify(await evaluate(`(async()=>{${await readFile(arg, "utf8")}})()`), null, 2));
  } else if (mode === "value") {
    console.log(await evaluate(`(() => { const e = document.querySelector(${JSON.stringify(arg)}); return e ? (e.value ?? e.textContent) : null; })()`));
  } else {
    console.log(
      JSON.stringify(
        await evaluate(`({ title: document.title, text: document.body.innerText, buttons: [...document.querySelectorAll("button")].map((b) => b.getAttribute("aria-label") ?? b.innerText), w: innerWidth, h: innerHeight })`),
        null,
        2,
      ),
    );
  }
} finally {
  ws.close();
}
