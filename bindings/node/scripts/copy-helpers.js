#!/usr/bin/env node
// Copies the baked helper binaries (sdframe, sdhid, sdax) into this package so a host needs
// nothing but this directory at runtime. Runs after `napi build`; idempotent.
'use strict';
const { execFileSync } = require('node:child_process');
const fs = require('node:fs');
const { join } = require('node:path');

const root = join(__dirname, '..', '..', '..');
console.log(execFileSync('cargo', ['build', '-p', 'simdriver', '--release'], { cwd: root, encoding: 'utf8' }).trim());

const HELPERS = ['sdframe', 'sdhid', 'sdax'];
const buildDir = join(root, 'target', 'release', 'build');
const out = fs
  .readdirSync(buildDir)
  .filter((d) => d.startsWith('simdriver-'))
  .map((d) => join(buildDir, d, 'out', 'helpers'))
  .filter((d) => HELPERS.every((bin) => fs.existsSync(join(d, bin))))
  .sort((a, b) => fs.statSync(b).mtimeMs - fs.statSync(a).mtimeMs)[0];
if (!out) throw new Error('baked helpers not found; cargo build -p simdriver --release');

const dest = join(__dirname, '..', 'helpers');
fs.mkdirSync(dest, { recursive: true });
for (const bin of HELPERS) {
  // Unlink first: overwriting a signed binary in place keeps its inode, and the kernel's
  // cached signature for that inode no longer matches, so every exec dies with SIGKILL.
  fs.rmSync(join(dest, bin), { force: true });
  fs.copyFileSync(join(out, bin), join(dest, bin));
  fs.chmodSync(join(dest, bin), 0o755);
}
console.log(`helpers copied from ${out} into ${dest}`);
