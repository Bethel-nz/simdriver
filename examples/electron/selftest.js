'use strict';
// Launches the example with --selftest (no simulator, no native addon needed): the window
// draws a synthetic red BGRA frame and main.js checks the pixel. Exit code is the result.
const { spawnSync } = require('node:child_process');
const electron = require('electron'); // path to the Electron binary

const run = spawnSync(electron, ['.', '--selftest'], { cwd: __dirname, stdio: 'inherit', timeout: 30000 });
if (run.error) {
  console.error('selftest could not run:', run.error.message);
  process.exit(1);
}
process.exit(run.status ?? 1);
