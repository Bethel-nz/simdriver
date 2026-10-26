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

/** An installed app (system apps included). */
export interface App {
  bundleId: string;
  name: string;
  /** `User` or `System`, as simctl reports it. */
  appType: string;
}

export function listDevices(): Device[];
/** Installed apps on a simulator. Blocks for the `simctl listapps` round trip. */
export function listApps(udid: string): App[];
/** Boot headlessly and wait until ready (blocks, up to 60 s). Already booted is success. */
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
  /** A text field's placeholder, when it reports one. */
  placeholder?: string;
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
  placeholder?: string;
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

/** A ref from the last snapshot (`e4`), a label or identifier on screen, or a grid cell (`C4`). */
export type AgentTarget = string;
type Direction = 'up' | 'down' | 'left' | 'right';
type HardwareButton = 'home' | 'lock' | 'side' | 'siri' | 'volume-up' | 'volume-down';

export type AgentStep =
  | { action: 'tap'; target: AgentTarget }
  | { action: 'doubleTap'; target: AgentTarget }
  /** iOS's long press (context menus, rearrange mode). `ms` defaults to 800. */
  | { action: 'longPress'; target: AgentTarget; ms?: number }
  /**
   * Clears the field (cmd+a, backspace), types `value`, then re-reads it into the
   * report's `verified`. `secret` keeps the value out of reports and errors.
   */
  | { action: 'fill'; target: AgentTarget; value: string; secret?: boolean }
  | { action: 'type'; value: string }
  /** A named key (`enter`, `backspace`, `tab`, arrows, `home`, `pageup`…) or a combo (`cmd+a`). */
  | { action: 'key'; name: string }
  | { action: 'press'; button: HardwareButton }
  /** Reveal what lies `direction`: `down` drags the content up. Avoids the keyboard. */
  | { action: 'scroll'; direction: Direction; target?: AgentTarget }
  /** A fast swipe with momentum. */
  | { action: 'fling'; direction: Direction }
  /** Targets or `[x, y]` points in 0..1 of the screen; `count` 1–10. */
  | {
      action: 'swipe';
      from: AgentTarget | [number, number];
      to: AgentTarget | [number, number];
      count?: number;
      pauseMs?: number;
    }
  /** Reorder handles, sliders. Holds `sourceHoldMs` (800) before moving over `moveMs` (500). */
  | {
      action: 'drag';
      from: AgentTarget;
      to: AgentTarget;
      sourceHoldMs?: number;
      moveMs?: number;
      destinationHoldMs?: number;
    }
  /** Two fingers around the target (screen centre by default). `scale` > 1 zooms in. */
  | { action: 'pinch'; target?: AgentTarget; scale: number }
  | { action: 'rotate'; target?: AgentTarget; degrees: number }
  /** Two-finger pan by `dx`, `dy` in screen fractions. */
  | { action: 'pan'; target?: AgentTarget; dx: number; dy: number; ms?: number }
  /** Pan, scale and rotate in one continuous two-finger stroke. */
  | { action: 'transform'; target?: AgentTarget; dx?: number; dy?: number; scale: number; degrees?: number }
  /** `system`: the left-edge swipe; `inApp`: tap the navigation bar's back button. */
  | { action: 'back'; mode: 'system' | 'inApp' }
  | { action: 'appSwitcher' }
  | { action: 'keyboardDismiss' }
  /** Waits until some label or value on screen contains `text` (case-insensitive). */
  | { action: 'waitFor'; text: string; timeoutMs?: number }
  /** `get` reads a system alert, `wait` polls for one (`ms`, default 5000); `accept`/`dismiss` tap a known button. */
  | { action: 'alert'; op: 'get' | 'wait' | 'accept' | 'dismiss'; ms?: number }
  | { action: 'screenshotElement'; target: AgentTarget }
  /** Accepts an "Open in …?" prompt that follows. */
  | { action: 'openUrl'; url: string }
  | { action: 'launch'; bundleId: string }
  | { action: 'terminate'; bundleId: string }
  | { action: 'setLocation'; lat: number; lon: number }
  | { action: 'appearance'; appearance: 'light' | 'dark' }
  | { action: 'clipboardRead' }
  | { action: 'clipboardWrite'; text: string };

export type AgentErrorCode =
  | 'stale'
  | 'covered'
  | 'not-found'
  | 'not-ready'
  | 'invalid'
  | 'device'
  | 'not-text-input'
  | 'commit-not-observed'
  | 'clear-not-observed'
  | 'keyboard-occludes';

export interface AgentAlert {
  title: string | null;
  message: string | null;
  buttons: string[];
}

export interface AgentStepsReport {
  ok: boolean;
  completed: number;
  failed: { index: number; step: AgentStep; code: AgentErrorCode; error: string } | null;
  /** One per completed `fill`: did the field read back exactly the value typed? */
  verified: boolean[];
  /** One per `clipboardRead`. */
  clipboard: string[];
  /** One per `screenshotElement`: the target's frame, cropped (PNG). */
  screenshots: { target: string; png: Buffer }[];
  /** Alerts read by `alert` steps, or accepted after `openUrl`. */
  alerts: AgentAlert[];
  /** The screen after the run (or the failure), without an image. Secret fill values are blanked. */
  snapshot: Omit<AgentSnapshot, 'image'> | null;
}

/** An agent refusal: `code` says why. */
export interface AgentError extends Error {
  code: AgentErrorCode;
}

/**
 * The agent layer. Refusals reject with an {@link AgentError}.
 */
export interface Agent {
  /** Waits for the screen to settle (up to `settleMs`, default 10000; a still screen takes ~100 ms). */
  snapshot(opts?: { settleMs?: number; image?: boolean; grid?: boolean; cols?: number }): Promise<AgentSnapshot>;
  /**
   * Tap a ref (`e4`) or grid cell (`C4`). A ref is re-read right before the touch and
   * refused if it moved, vanished or is covered — or, with `expect`, no longer reads as that.
   */
  tap(target: string, opts?: { expect?: string }): Promise<{ target: string; name: string; at: [number, number] }>;
  /** Several actions in one call; stops at the first failure. Never rejects for a failed step. */
  steps(steps: AgentStep[]): Promise<AgentStepsReport>;
}

/**
 * simctl privacy services: `all`, `calendar`, `contacts-limited`, `contacts`, `location`,
 * `location-always`, `photos-add`, `photos`, `media-library`, `microphone`, `motion`,
 * `reminders`, `siri` (and `camera` where the runtime supports it).
 */
export type PrivacyService = string;

export interface Driver {
  readonly udid: string;
  readonly state: DriverState;

  // Touch and gestures — coordinates are 0..1 in the box.
  pointer(phase: 'down' | 'move' | 'up', x: number, y: number, mods?: Modifiers): void;
  scroll(
    phase: 'began' | 'changed' | 'ended' | 'momentum',
    x: number,
    y: number,
    dx: number,
    dy: number,
  ): void;
  /** A pointer down and up at (x, y). */
  tap(x: number, y: number): void;
  /** Two taps 100 ms apart; resolves after the second. */
  doubleTap(x: number, y: number): Promise<void>;
  /** Down, hold `ms` (default 800), up. The hold runs off the JS thread; frames keep flowing. */
  longPress(x: number, y: number, ms?: number): Promise<void>;
  /** Down, hold `holdMs` (800), move over `moveMs` (500), hold `destinationHoldMs` (0), up. */
  drag(
    x1: number,
    y1: number,
    x2: number,
    y2: number,
    opts?: { holdMs?: number; moveMs?: number; destinationHoldMs?: number },
  ): Promise<void>;
  /** Two fingers placed exactly (rotation, off-centre pinch). */
  touch2(phase: 'down' | 'move' | 'up', x1: number, y1: number, x2: number, y2: number): void;

  // Keyboard.
  /**
   * A named key (`enter`, `escape`, `backspace`, `tab`, `space`, arrows, `home`, `end`,
   * `pageup`, `pagedown`, single letters and digits) or a combo like `cmd+a`. False if unknown.
   */
  key(name: string): boolean;
  /** The last key pressed while holding the others (`cmd`, `shift`, `alt`, `ctrl`). */
  keyCombo(keys: string[]): boolean;
  /**
   * Types ASCII as keystrokes. Other text (emoji, accents, CJK) goes through the
   * pasteboard and cmd+v, so the app sees a paste.
   */
  text(s: string): void;
  button(name: HardwareButton, presses?: number, holdSecs?: number): void;

  // Screen.
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
  /** The device screen in points (e.g. 402×874), once input is connected; else null. */
  screenPoints(): { width: number; height: number } | null;
  stats(): Stats;

  // Device controls (simctl, simulator only). Each runs off the JS thread.
  openUrl(url: string): Promise<void>;
  launch(bundleId: string): Promise<void>;
  terminate(bundleId: string): Promise<void>;
  apps(): Promise<App[]>;
  /** Terminates the app and deletes the contents of its data container. Destructive. */
  clearAppData(bundleId: string): Promise<void>;
  clipboardRead(): Promise<string>;
  /** An empty string clears the pasteboard. */
  clipboardWrite(text: string): Promise<void>;
  setLocation(lat: number, lon: number): Promise<void>;
  setAppearance(appearance: 'light' | 'dark'): Promise<void>;
  grantPermission(service: PrivacyService, bundleId: string): Promise<void>;
  revokePermission(service: PrivacyService, bundleId: string): Promise<void>;
  resetPermission(service: PrivacyService, bundleId: string): Promise<void>;
  /** Dynamic Type: `extra-small` … `extra-extra-extra-large`, `accessibility-medium` … `accessibility-extra-extra-extra-large`, or `increment`/`decrement`. */
  setContentSize(size: string): Promise<void>;
  /** A clean status bar for screenshots: 9:41, full battery, charged. */
  statusBarOverride(): Promise<void>;
  statusBarClear(): Promise<void>;
  /** Delivers an APNs payload (a JSON file path) to the app. */
  pushNotification(bundleId: string, payloadPath: string): Promise<void>;
  resetKeychain(): Promise<void>;

  /** Always call it: stops the helpers. The final 'stopped' state is not delivered. */
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
   * Never boots a simulator: boot one first (or pass a booted `udid`).
   */
  start(opts: DriverOptions, onEvent: (e: DriverEvent) => void): Promise<Driver>;
};
