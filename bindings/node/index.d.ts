/**
 * Embed a live iOS simulator: frames (BGRA Buffers) + touch input in 0..1 box
 * coordinates. Loads in Node, Bun and Electron's main process.
 *
 * Hand-written: this is the public API (index.js wraps the native module). The napi
 * build writes the raw native types to native.d.ts instead (see package.json).
 */
export interface Device {
  udid: string;
  name: string;
  state: string;
  deviceType: string;
  runtime: string;
}
export function listDevices(): Device[];
export function boot(udid: string): void;
export function shutdown(udid: string): void;

export type DriverState = 'starting' | 'live' | 'input-displaced' | 'restarting' | 'stopped';

export type DriverEvent =
  | {
      type: 'frame';
      width: number;
      height: number;
      sourceWidth: number;
      sourceHeight: number;
      seq: number;
      /** Tightly packed BGRA; valid until the next event. */
      data: Buffer;
    }
  | { type: 'state'; state: DriverState }
  | { type: 'note'; text: string };

export interface Modifiers {
  alt?: boolean;
  shift?: boolean;
  ctrl?: boolean;
  meta?: boolean;
}

export interface Stats {
  fps: number;
  activeFps: number;
  idleFps: number;
  lastResponseMs: number | null;
  maxActiveGapMs: number;
  frames: number;
}

/** One element of the accessibility tree. `frame` is [x, y, width, height] in screen points. */
export interface AxNode {
  role: string;
  label?: string;
  value?: string;
  /** The app's accessibilityIdentifier, when set. */
  id?: string;
  enabled: boolean;
  frame: [number, number, number, number];
  children?: AxNode[];
}

/**
 * A named grid over the screen for what accessibility does not expose: rows are letters
 * down the screen (A, B, C, D, F, … then AA; never E, so no cell reads like an element
 * ref), columns numbers across. "C4" = row C, column 4.
 * Coordinates are 0..1 of the screen, like `pointer`.
 */
export class Grid {
  constructor(screenWidth: number, screenHeight: number, cols?: number);
  readonly rows: number;
  readonly cols: number;
  name(row: number, col: number): string;
  center(name: string): { x: number; y: number } | null;
  rect(name: string): { x: number; y: number; width: number; height: number } | null;
  at(x: number, y: number): string;
}

/** One interactive element in an agent snapshot. Frames and points are screen points. */
export interface AgentElement {
  /** `e4`: stable while the element stays on screen, never reused for another. */
  ref: string;
  /** `button`, `textfield`, `switch`, … */
  role: string;
  label?: string;
  value?: string;
  id?: string;
  frame: [number, number, number, number];
  /** Where a tap lands; null when covered. */
  tap: [number, number] | null;
  visibility: 'visible' | 'partial' | 'covered';
}

export interface AgentSnapshot {
  id: number;
  /** 'grid' when nothing on screen is accessible (maps, canvases): target cells like C4. */
  mode: 'elements' | 'grid';
  screen: [number, number];
  elements: AgentElement[];
  /** Interactive elements wholly off screen (scroll to reach). */
  offscreen: number;
  grid: { rows: number; cols: number };
  /** False if the screen was still changing when the settle time ran out. */
  settled: boolean;
  /** The snapshot as text for a model: one line per element. */
  text: string;
  /** Annotated screenshot (PNG): boxes and refs, or the grid. */
  image: Buffer | null;
}

export type AgentStep =
  | { action: 'tap'; target: string }
  | { action: 'fill'; target: string; value: string }
  | { action: 'scroll'; direction: 'up' | 'down' | 'left' | 'right'; target?: string }
  | { action: 'type'; value: string }
  | { action: 'key'; name: string }
  | { action: 'press'; button: 'home' | 'lock' | 'side' | 'siri' | 'volume-up' | 'volume-down' }
  | { action: 'waitFor'; text: string; timeoutMs?: number };

export interface AgentStepsReport {
  ok: boolean;
  completed: number;
  failed: { index: number; step: AgentStep; error: string } | null;
  /** The screen after the run (or the failure), without an image. */
  snapshot: Omit<AgentSnapshot, 'image'> | null;
}

/**
 * The agent layer. Refusals reject with an Error whose `code` is `stale`, `covered`,
 * `not-found`, `not-ready`, `invalid` or `device`.
 */
export interface Agent {
  /** Waits for the screen to settle (up to `settleMs`, default 10000; a still screen takes ~100 ms). */
  snapshot(opts?: { settleMs?: number; image?: boolean; grid?: boolean; cols?: number }): Promise<AgentSnapshot>;
  /**
   * Tap a ref (`e4`) or grid cell (`C4`). A ref is re-read right before the touch and
   * refused if it moved, vanished or is covered — or, with `expect`, no longer reads as that.
   */
  tap(target: string, opts?: { expect?: string }): Promise<{ target: string; name: string; at: [number, number] }>;
  /** Several actions in one call; stops at the first failure. Targets: refs, labels or cells. */
  steps(steps: AgentStep[]): Promise<AgentStepsReport>;
}

export interface Driver {
  readonly udid: string;
  readonly state: DriverState;
  pointer(phase: 'down' | 'move' | 'up', x: number, y: number, mods?: Modifiers): void;
  scroll(
    phase: 'began' | 'changed' | 'ended' | 'momentum',
    x: number,
    y: number,
    dx: number,
    dy: number,
  ): void;
  /** A pointer down and up at (x, y), 0..1 in the box. */
  tap(x: number, y: number): void;
  /**
   * The frontmost app's accessibility tree, home screen included. The first call takes
   * 1–2 s (the helper starts and the guest warms up); later ones tens of ms. Rejects on
   * an Xcode without the private API it uses.
   */
  accessibility(): Promise<AxNode>;
  /** The agent layer (one per driver). */
  agent(): Agent;
  /** A Grid over this device's screen, or null before input connects. */
  grid(cols?: number): Grid | null;
  /** Two fingers placed exactly (rotation, off-centre pinch), each 0..1 in the box. */
  touch2(phase: 'down' | 'move' | 'up', x1: number, y1: number, x2: number, y2: number): void;
  /** The device screen in points (e.g. 402×874), once input is connected; else null. */
  screenPoints(): { width: number; height: number } | null;
  key(name: string): boolean;
  text(s: string): void;
  button(
    name: 'home' | 'lock' | 'side' | 'siri' | 'volume-up' | 'volume-down',
    presses?: number,
    holdSecs?: number,
  ): void;
  stats(): Stats;
  stop(): void;
}

export interface DriverOptions {
  udid?: string;
  fps?: number;
  scale?: number;
  helperDir?: string;
}

export const Driver: {
  /**
   * Resolves once frames stream and input is connected (or after a bounded wait, still
   * 'starting'). The wait runs off the JS thread; events may arrive before it resolves.
   */
  start(opts: DriverOptions, onEvent: (e: DriverEvent) => void): Promise<Driver>;
};
