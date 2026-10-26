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
 * down the screen (A, B, … then AA), columns numbers across. "C4" = row C, column 4.
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
