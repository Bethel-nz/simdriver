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

  static start(opts, onEvent) {
    if (typeof onEvent !== 'function') throw new TypeError('onEvent must be a function');
    const o = opts ?? {};
    const n = native.Driver.start(
      {
        // undefined, not null: napi maps a missing field to None but rejects null.
        udid: o.udid ?? undefined,
        fps: o.fps ?? 60,
        scale: o.scale ?? 2,
        helperDir: o.helperDir ?? defaultHelperDir,
      },
      // The pump: drains parked events (latest frame wins) and re-arms; see src/lib.rs.
      () => {
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

module.exports = {
  listDevices: native.listDevices,
  boot: native.boot,
  shutdown: native.shutdown,
  Driver,
};
