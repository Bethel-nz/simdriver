// Task 11 smoke test — run with `bun test` or `node --import tsx test/node.test.mjs`.
// Without SIMDRIVER_IT=1 only the local parts run; with it, a driver is started on the
// booted test simulator (SIMDRIVER_TEST_UDID, else first booted) over anim.html.
import { readFileSync } from 'node:fs';
import { createServer } from 'node:http';
import { execFileSync, spawnSync } from 'node:child_process';
import { join, dirname } from 'node:path';
import { fileURLToPath } from 'node:url';
import { describe, expect, test } from './runner.ts';
import { Driver, listDevices } from '../index.js';

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

  test('Driver.start streams frames and stop() delivers nothing after', async () => {
    if (!IT) return; // gated: needs a booted simulator

    const udid = bootedUdid().udid;
    const page = await openPage(udid, 'anim.html');

    const events: any[] = [];
    const driver = Driver.start({ udid }, (e) => events.push(e));
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
