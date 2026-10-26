'use strict';
// Loads the platform binary produced by `napi build --platform --no-js`
// (simdriver.<platform>.node) and presents the plan's public API: frames as BGRA
// Buffers, input in 0..1 box coordinates. Hand-written — `napi build` must run with
// --no-js or it overwrites this file with its template.
const { join } = require('node:path');
const fs = require('node:fs');

const bindingName = `simdriver.${process.platform}-${process.arch}.node`;
const local = join(__dirname, bindingName);
const native = fs.existsSync(local)
  ? require(local)
  : require(`simdriver-${process.platform}-${process.arch}`);

// The package ships the helper binaries next to index.js when built via `npm run build`
// (scripts/copy-helpers.js). Passing helperDir explicitly lets hosts bundle their own.
const bundledHelpers = join(__dirname, 'helpers');
const defaultHelperDir = fs.existsSync(bundledHelpers) ? bundledHelpers : undefined;

const modifiers = (mods) => ({
  alt: Boolean(mods?.alt),
  shift: Boolean(mods?.shift),
  ctrl: Boolean(mods?.ctrl),
  meta: Boolean(mods?.meta),
});

class Driver {
  #n;

  constructor(nativeDriver) {
    this.#n = nativeDriver;
  }

  static async start(opts, onEvent) {
    if (typeof onEvent !== 'function') throw new TypeError('onEvent must be a function');
    const o = opts ?? {};
    // Events can arrive before start resolves: the pump then leaves them parked (the
    // native side holds them until pumpDone) and drains once the driver exists.
    let n;
    let parked = false;
    n = await native.Driver.start(
      {
        // undefined, not null: napi maps a missing field to None but rejects null.
        udid: o.udid ?? undefined,
        fps: o.fps ?? 60,
        scale: o.scale ?? 2,
        helperDir: o.helperDir ?? defaultHelperDir,
      },
      // The pump: drains parked events (latest frame wins) and re-arms; see src/lib.rs.
      () => {
        if (!n) {
          parked = true;
          return;
        }
        try {
          for (;;) {
            const ev = n.takeEvent();
            if (!ev) break;
            if (ev.type === 'frame') {
              onEvent({
                type: 'frame',
                width: ev.width,
                height: ev.height,
                sourceWidth: ev.sourceWidth,
                sourceHeight: ev.sourceHeight,
                seq: ev.seq,
                data: ev.data,
              });
            } else if (ev.type === 'state') {
              onEvent({ type: 'state', state: ev.state });
            } else {
              onEvent({ type: 'note', text: ev.text });
            }
          }
        } finally {
          n.pumpDone();
        }
      },
    );
    if (parked) n.pumpDone();
    return new Driver(n);
  }

  get udid() {
    return this.#n.udid;
  }

  get state() {
    return this.#n.state;
  }

  pointer(phase, x, y, mods) {
    this.#n.pointer(phase, x, y, modifiers(mods));
  }

  scroll(phase, x, y, dx, dy) {
    this.#n.scroll(phase, x, y, dx, dy);
  }

  tap(x, y) {
    this.#n.tap(x, y);
  }

  /** The frontmost app's accessibility tree (frames in screen points). */
  async accessibility() {
    return JSON.parse(await this.#n.accessibility());
  }

  /** A Grid over this screen (`cols` columns, default 6), or null before input connects. */
  grid(cols) {
    const s = this.#n.screenPoints();
    return s ? new native.Grid(s.width, s.height, cols ?? undefined) : null;
  }

  touch2(phase, x1, y1, x2, y2) {
    this.#n.touch2(phase, x1, y1, x2, y2);
  }

  screenPoints() {
    return this.#n.screenPoints() ?? null;
  }

  key(name) {
    return this.#n.key(name);
  }

  text(s) {
    this.#n.text(s);
  }

  button(name, presses = 1, holdSecs = 0) {
    this.#n.button(name, presses, holdSecs);
  }

  stats() {
    return this.#n.stats();
  }

  stop() {
    this.#n.stop();
  }
}

// One `exports.x =` per name: Node's ESM loader finds CommonJS named exports by
// scanning for this shape, so `import { Driver } from 'simdriver'` works there too.
exports.listDevices = native.listDevices;
exports.boot = native.boot;
exports.shutdown = native.shutdown;
exports.Driver = Driver;
exports.Grid = native.Grid;
