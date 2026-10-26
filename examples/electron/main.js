'use strict';
// Main process: owns the driver. Frames go to the renderer with webContents.postMessage
// (one structured-clone copy each); input comes back over ipc and goes straight to the
// driver. `--selftest` skips the driver and checks the canvas path with a synthetic frame.
const { app, BrowserWindow, ipcMain } = require('electron');
const path = require('node:path');

const SELFTEST = process.argv.includes('--selftest');
let driver = null;

function createWindow() {
  const win = new BrowserWindow({
    width: 430,
    height: 900,
    title: 'simdriver',
    backgroundColor: '#111111',
    show: !SELFTEST,
    webPreferences: { preload: path.join(__dirname, 'preload.js') },
  });
  win.loadFile('index.html');
  return win;
}

async function startDriver(win) {
  // Loaded here, not at the top: the selftest must run without the native addon built.
  const { Driver } = require('simdriver');
  const udid = process.env.SIMDRIVER_UDID || undefined;
  driver = await Driver.start({ udid }, (e) => {
    if (win.isDestroyed()) return;
    if (e.type === 'frame') {
      // e.data is only valid until the next event; postMessage clones it now.
      win.webContents.postMessage('frame', { width: e.width, height: e.height, data: e.data });
    } else if (e.type === 'state') {
      win.webContents.send('state', e.state);
    }
  });
  win.webContents.send('state', driver.state);
}

// Renderer → driver. Only the calls the page needs; anything else is ignored.
const CALLS = new Set(['pointer', 'scroll', 'key', 'text', 'button']);
ipcMain.on('input', (_e, call, args) => {
  if (driver && CALLS.has(call)) driver[call](...args);
});

async function selftest(win) {
  // A 4×4 frame of BGRA 00 00 FF FF must draw as pure red.
  const data = new Uint8Array(4 * 4 * 4);
  for (let i = 0; i < data.length; i += 4) data.set([0x00, 0x00, 0xff, 0xff], i);
  const rect = await new Promise((resolve) => {
    ipcMain.once('selftest-drawn', (_e, r) => resolve(r));
    win.webContents.postMessage('frame', { width: 4, height: 4, data });
  });
  const x = Math.floor(rect.x + rect.width / 2);
  const y = Math.floor(rect.y + rect.height / 2);
  const img = await win.webContents.capturePage({ x, y, width: 1, height: 1 });
  const px = img.toBitmap(); // BGRA on macOS
  const [r, g, b] = [px[2], px[1], px[0]];
  // capturePage returns the display's colour space (sRGB red is rgb(234,51,35) on a P3
  // screen, and force-color-profile does not change that), so check "clearly red": that
  // still fails a channel swap (blue) or a frame that never drew (black).
  const ok = r >= 200 && g <= 80 && b <= 80;
  console.log(`selftest: centre pixel rgb(${r},${g},${b}) ${ok ? 'ok (red)' : 'expected red'}`);
  app.exit(ok ? 0 : 1);
}

app.whenReady().then(() => {
  const win = createWindow();
  win.webContents.once('did-finish-load', () => {
    if (SELFTEST) {
      selftest(win).catch((err) => {
        console.error('selftest failed:', err);
        app.exit(1);
      });
      return;
    }
    startDriver(win).catch((err) => {
      if (!win.isDestroyed()) win.webContents.send('state', `error: ${err.message}`);
    });
  });
});

app.on('window-all-closed', () => {
  driver?.stop();
  driver = null;
  app.quit();
});
