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
  start(opts: DriverOptions, onEvent: (e: DriverEvent) => void): Driver;
};
