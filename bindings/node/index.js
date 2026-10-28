'use strict';
// Loads the platform binary produced by `napi build --platform --no-js`
// (simdriver.<platform>.node) and presents the plan's public API: frames as BGRA
// Buffers, input in 0..1 box coordinates. Hand-written — `napi build` must run with
// --no-js or it overwrites this file with its template.
const { dirname, join } = require('node:path');
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

// Agent refusals arrive as "code: message"; expose the code for hosts to branch on.
const agentError = (e) => {
  const m = /^(stale|covered|not-found|ambiguous|not-ready|invalid|device|not-text-input|commit-not-observed|clear-not-observed|keyboard-occludes): /.exec(e?.message ?? '');
  if (m) e.code = m[1];
  return e;
};

class Agent {
  #n;

  constructor(nativeAgent) {
    this.#n = nativeAgent;
  }

  async snapshot(opts = {}) {
    try {
      const r = await this.#n.snapshot(
        opts.settleMs ?? undefined,
        opts.image ?? undefined,
        opts.grid ?? undefined,
        opts.cols ?? undefined,
      );
      return { ...JSON.parse(r.json), image: r.image ?? null };
    } catch (e) {
      throw agentError(e);
    }
  }

  /** The screen as a plain PNG Buffer; with `path`, also written there (folders created). */
  async screenshot(path) {
    let png;
    try {
      png = await this.#n.screenshot();
    } catch (e) {
      throw agentError(e);
    }
    if (path) {
      await fs.promises.mkdir(dirname(path), { recursive: true });
      await fs.promises.writeFile(path, png);
    }
    return png;
  }

  async tap(target, opts = {}) {
    try {
      return JSON.parse(await this.#n.tap(target, opts.expect ?? undefined));
    } catch (e) {
      throw agentError(e);
    }
  }

  async steps(steps) {
    const { report: json, sheets, images } = await this.#n.steps(JSON.stringify(steps));
    const report = JSON.parse(json);
    for (const screenshot of report.screenshots ?? []) screenshot.png = Buffer.from(screenshot.png);
    const record = report.record;
    if (record) {
      record.sheets = images.slice(0, sheets);
      record.frames.forEach((frame, i) => {
        frame.png = images[sheets + i];
      });
    }
    return report;
  }

  /** Record the screen around `steps` (none = passively): steps with a leading record step. */
  record(options, steps = []) {
    return this.steps([{ ...options, action: 'record' }, ...steps]);
  }
}

class Driver {
  #n;
  #agent;

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

  doubleTap(x, y) {
    return this.#n.doubleTap(x, y);
  }

  /** The agent layer for this driver (one per driver: refs carry across calls). */
  agent() {
    this.#agent ??= new Agent(new native.Agent(this.#n));
    return this.#agent;
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

  longPress(x, y, ms) {
    return this.#n.longPress(x, y, ms ?? undefined);
  }

  drag(x1, y1, x2, y2, opts) {
    return this.#n.drag(
      x1,
      y1,
      x2,
      y2,
      opts?.holdMs ?? undefined,
      opts?.moveMs ?? undefined,
      opts?.destinationHoldMs ?? undefined,
    );
  }

  key(name) {
    return this.#n.key(name);
  }

  keyCombo(keys) {
    return this.#n.keyCombo(keys);
  }

  clipboardRead() {
    return this.#n.clipboardRead();
  }

  clipboardWrite(text) {
    return this.#n.clipboardWrite(text);
  }

  openUrl(url) {
    return this.#n.openUrl(url);
  }

  launch(bundleId) {
    return this.#n.launch(bundleId);
  }

  terminate(bundleId) {
    return this.#n.terminate(bundleId);
  }

  setLocation(lat, lon) {
    return this.#n.setLocation(lat, lon);
  }

  setAppearance(appearance) {
    return this.#n.setAppearance(appearance);
  }

  grantPermission(service, bundleId) {
    return this.#n.grantPermission(service, bundleId);
  }

  revokePermission(service, bundleId) {
    return this.#n.revokePermission(service, bundleId);
  }

  resetPermission(service, bundleId) {
    return this.#n.resetPermission(service, bundleId);
  }

  setContentSize(size) {
    return this.#n.setContentSize(size);
  }

  statusBarOverride() {
    return this.#n.statusBarOverride();
  }

  statusBarClear() {
    return this.#n.statusBarClear();
  }

  pushNotification(bundleId, payloadPath) {
    return this.#n.pushNotification(bundleId, payloadPath);
  }

  resetKeychain() {
    return this.#n.resetKeychain();
  }

  apps() {
    return this.#n.apps();
  }

  clearAppData(bundleId) {
    return this.#n.clearAppData(bundleId);
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
exports.listApps = native.listApps;
exports.boot = native.boot;
exports.shutdown = native.shutdown;
exports.Driver = Driver;
exports.Grid = native.Grid;
