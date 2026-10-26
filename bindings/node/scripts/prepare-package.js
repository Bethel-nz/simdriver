'use strict';
const fs = require('node:fs');
const path = require('node:path');

const dest = path.resolve(__dirname, '..');
const root = path.resolve(dest, '../..');
for (const file of ['simdriver.darwin-arm64.node', 'helpers/sdframe', 'helpers/sdhid', 'helpers/sdax']) {
  if (!fs.existsSync(path.join(dest, file))) throw new Error(`Missing ${file}; run npm run build before packing`);
}
for (const file of ['LICENSE', 'NOTICE']) fs.copyFileSync(path.join(root, file), path.join(dest, file));
fs.copyFileSync(path.join(root, 'crates/simdriver/helpers/private-headers/LICENSE'), path.join(dest, 'THIRD-PARTY-LICENSE'));
fs.copyFileSync(path.join(root, 'docs/simdriver-controls.md'), path.join(dest, 'simdriver-controls.md'));
const readme = fs.readFileSync(path.join(root, 'README.md'), 'utf8').replace('](docs/simdriver-controls.md)', '](simdriver-controls.md)');
fs.writeFileSync(path.join(dest, 'README.md'), readme);
