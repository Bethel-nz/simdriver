'use strict';
// The page's only door to the main process: frames and state in, input out.
const { contextBridge, ipcRenderer } = require('electron');

contextBridge.exposeInMainWorld('simdriver', {
  onFrame: (cb) => ipcRenderer.on('frame', (_e, frame) => cb(frame)),
  onState: (cb) => ipcRenderer.on('state', (_e, state) => cb(state)),
  selftestDrawn: (rect) => ipcRenderer.send('selftest-drawn', rect),
  pointer: (phase, x, y, mods) => ipcRenderer.send('input', 'pointer', [phase, x, y, mods]),
  scroll: (phase, x, y, dx, dy) => ipcRenderer.send('input', 'scroll', [phase, x, y, dx, dy]),
  // Fire-and-forget over ipc, so the page can't learn whether a key name was known.
  key: (name) => (ipcRenderer.send('input', 'key', [name]), true),
  text: (s) => ipcRenderer.send('input', 'text', [s]),
  button: (name, presses, holdSecs) => ipcRenderer.send('input', 'button', [name, presses, holdSecs]),
});
