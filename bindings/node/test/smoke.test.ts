// Task 11 smoke test — run with `bun test` or `node --import tsx test/node.test.mjs`.
// Without SIMDRIVER_IT=1 only the local parts run; with it, a driver is started on the
// booted test simulator (SIMDRIVER_TEST_UDID, else first booted) over anim.html.
import { readFileSync } from 'node:fs';
import { createServer } from 'node:http';
import { execFileSync, spawnSync } from 'node:child_process';
import { join, dirname } from 'node:path';
import { fileURLToPath } from 'node:url';
import { describe, expect, test } from './runner.ts';
import { Driver, Grid, listDevices } from '../index.js';

const IT = process.env.SIMDRIVER_IT === '1';
const here = dirname(fileURLToPath(import.meta.url));

async function openPage(udid: string, fixture: string) {
  const body = readFileSync(join(here, '..', '..', '..', 'crates', 'simdriver', 'tests', 'fixtures', fixture));
  const server = createServer((_req, res) => {
    res.writeHead(200, { 'Content-Type': 'text/html', 'Cache-Control': 'no-store', Connection: 'close' });
    res.end(body);
  });
  server.listen(0, '127.0.0.1');
  await new Promise<void>((resolve) => server.once('listening', resolve));
  const port = (server.address() as { port: number }).port;
  const url = `http://127.0.0.1:${port}/`;
  // Ignore failure: Safari not running is the normal case here.
  spawnSync('xcrun', ['simctl', 'terminate', udid, 'com.apple.mobilesafari'], { stdio: 'ignore' });
  execFileSync('xcrun', ['simctl', 'launch', udid, 'com.apple.mobilesafari', url], { stdio: 'ignore' });
  return { server };
}

function bootedUdid(): { udid: string } {
  const out = execFileSync('xcrun', ['simctl', 'list', 'devices', 'booted', '-j'], { encoding: 'utf8' });
  const devices = Object.values(JSON.parse(out).devices).flat() as { udid: string }[];
  const wanted = process.env.SIMDRIVER_TEST_UDID;
  return (wanted && devices.find((d) => d.udid === wanted)) || devices[0];
}

describe('simdriver', () => {
  test('listDevices returns the local devices', () => {
    const devices = listDevices();
    expect(devices.length).toBeGreaterThan(0);
    for (const key of ['udid', 'name', 'state', 'deviceType', 'runtime']) {
      expect(key in devices[0]).toBeTruthy();
    }
  });

  test('Grid names cells and finds them again', () => {
    const g = new Grid(402, 874);
    expect(g.rows).toBe(13);
    expect(g.cols).toBe(6);
    expect(g.name(2, 3)).toBe('C4');
    const c = g.center('c4');
    expect(c !== null && g.at(c.x, c.y) === 'C4').toBe(true);
    expect(g.center('O1')).toBe(null);
    expect(g.center('E1')).toBe(null);
  });

  test('Driver.start({}) accepts missing options (no null reaches napi)', async () => {
    // A bogus helperDir makes start fail after the options convert, without a simulator:
    // with a device it is "helper missing", without one "no device" — never a
    // "Failed to convert JavaScript value `Null`" from an unset udid.
    let message = '';
    try {
      (await Driver.start({ helperDir: '/nonexistent/simdriver-helpers' }, () => {})).stop();
    } catch (e) {
      message = (e as Error).message;
    }
    expect(message.includes('Failed to convert')).toBe(false);
  });

  test('Driver.start streams frames and stop() delivers nothing after', async () => {
    if (!IT) return; // gated: needs a booted simulator

    const udid = bootedUdid().udid;
    const page = await openPage(udid, 'anim.html');

    const events: any[] = [];
    // start resolves off the JS thread: a timer keeps ticking while it waits.
    let ticks = 0;
    const timer = setInterval(() => ticks++, 10);
    const driver = await Driver.start({ udid }, (e) => events.push(e));
    clearInterval(timer);
    expect(ticks).toBeGreaterThan(0);
    expect(driver.udid).toBe(udid);

    const deadline = Date.now() + 10_000;
    while (events.filter((e) => e.type === 'frame').length < 100 && Date.now() < deadline) {
      await new Promise((r) => setTimeout(r, 50));
    }
    const frames = events.filter((e) => e.type === 'frame');
    expect(frames.length).toBeGreaterThanOrEqual(100);
    for (const f of frames.slice(0, 5)) {
      expect(f.data.length).toBe(f.width * f.height * 4);
      expect(f.seq).toBeGreaterThan(0);
    }
    const states = events.filter((e) => e.type === 'state').map((e) => e.state);
    expect(states).toContain('starting');
    expect(states).toContain('live');

    const pts = driver.screenPoints();
    expect(pts !== null && pts.width > 0 && pts.height > pts.width).toBe(true);

    const agent = driver.agent();
    expect(agent).toBe(driver.agent());
    const snap = await agent.snapshot({ settleMs: 3000 });
    expect(snap.id).toBe(1);
    expect(typeof snap.text).toBe('string');
    expect(snap.image === null || snap.image.subarray(1, 4).toString() === 'PNG').toBe(true);
    let code = '';
    try {
      await agent.tap('e999999');
    } catch (e) {
      code = (e as { code?: string }).code ?? '';
    }
    expect(code).toBe('not-found');

    const root = await driver.accessibility();
    expect(root.role).toBe('AXApplication');
    expect(root.frame.length).toBe(4);
    expect(driver.grid()?.rows).toBeGreaterThan(0);

    const stats = driver.stats();
    expect(stats.frames).toBeGreaterThanOrEqual(100);
    expect(stats.fps).toBeGreaterThan(0);

    driver.stop();
    driver.stop(); // idempotent
    const n = events.length;
    await new Promise((r) => setTimeout(r, 300));
    expect(events.length).toBe(n); // nothing after stop

    page.server.close();
    page.server.closeAllConnections?.();
  });
});
