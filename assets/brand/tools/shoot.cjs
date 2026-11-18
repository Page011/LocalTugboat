// electron shoot.cjs <out.png> <page.html> <width> <height> [transparent]
// Renders one local HTML page to a PNG (one page per run). Adapted from forteops plans/ui-examples/admin/shoot.cjs.
const { app, BrowserWindow } = require('electron')
const fs = require('fs')
const args = process.argv.slice(2).filter((x) => !x.startsWith('--'))
const [out, page, w, h, transparent] = args
app.disableHardwareAcceleration()
app.commandLine.appendSwitch('disable-lcd-text')
app.commandLine.appendSwitch('force-device-scale-factor', '1')
app.whenReady().then(async () => {
  const win = new BrowserWindow({
    show: false, width: +w, height: +h, useContentSize: true, frame: false,
    transparent: transparent === 'transparent',
    backgroundColor: transparent === 'transparent' ? '#00000000' : '#ffffff',
    webPreferences: { offscreen: true },
  })
  try {
    await win.loadFile(page)
    await new Promise((r) => setTimeout(r, 450))
    const img = await win.webContents.capturePage()
    const sz = img.getSize()
    fs.writeFileSync(out, img.toPNG())
    console.log('SHOT', out, sz.width + 'x' + sz.height)
  } catch (e) {
    console.error('FAIL', page, e && e.message)
    process.exitCode = 1
  }
  win.destroy()
  app.quit()
})
