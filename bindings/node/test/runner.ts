// One test file, two runtimes (plan Task 11 Step 4). Both Bun and Node understand
// node:test / node:assert (Bun aliases node:test to bun:test), so the runner needs no
// dynamic imports and no top-level await — which tsx's CJS transform cannot handle.
import * as nodeTest from 'node:test';
import * as assert from 'node:assert/strict';

export const describe = nodeTest.describe;
export const test = (name: string, fn: () => void | Promise<void>) =>
  nodeTest.test(name, fn);

export const expect = (actual: unknown) => {
  const str = (v: unknown) => JSON.stringify(v);
  return {
    toBe(want: unknown) {
      assert.strictEqual(actual, want, `expected ${str(actual)} to be ${str(want)}`);
    },
    toBeTruthy() {
      assert.ok(actual, `expected ${str(actual)} to be truthy`);
    },
    toBeGreaterThan(n: number) {
      assert.ok((actual as number) > n, `expected ${str(actual)} > ${n}`);
    },
    toBeGreaterThanOrEqual(n: number) {
      assert.ok((actual as number) >= n, `expected ${str(actual)} >= ${n}`);
    },
    toContain(item: unknown) {
      assert.ok(
        Array.isArray(actual) && actual.includes(item),
        `expected ${str(actual)} to contain ${str(item)}`,
      );
    },
  };
};
