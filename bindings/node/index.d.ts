/* Hand-written to match the index.js wrapper, not the napi surface underneath. */
/* eslint-disable */

/** Everything `snapshot()` reports. `text` is the numbered element list; `mode` is `elements` or `grid`. */
export type Snapshot = { text: string; image: Buffer | null; settled: boolean; mode: string; elements: unknown[] } & Record<string, unknown>
export type TapReport = { target: string; name: string; at: [number, number] }
export type StepFailure = { index: number; code: string; error: string }
export type RecordOptions = { timeMs?: number; idleMs?: number; full?: boolean; fps?: number; maxFrames?: number }
export type Recording = {
  /** Where the sheets were written; empty here (only the CLI writes files). */
  sheetFiles: string[]
  ended: 'idle' | 'time' | 'stepFailed' | 'error'
  extended: boolean
  requestedMs: number
  durationMs: number
  actionStartMs: number | null
  actionEndMs: number | null
  settledMs: number | null
  /** `file`: where the PNG was written; null here (only the CLI writes files). */
  frames: { index: number; tMs: number; change: number; png: Buffer; file: string | null }[]
  /** Contact sheets (PNG), usually one. */
  sheets: Buffer[]
  health: { received: number; sampled: number; skippedByPipeline: number; skippedByRecorder: number; maxGapMs: number; gapsOver34ms: number }
  error: string | null
} & Record<string, unknown>
export type StepsReport = { ok: boolean; completed: number; failed: StepFailure | null; screenshots?: { target: string; png: Buffer }[]; snapshot: { text: string } | null; record?: Recording } & Record<string, unknown>

/**
 * The agent layer over one driver: element refs, annotated screenshots, validated taps,
 * steps. Every call runs off the JS thread. Reach it through `driver.agent()`; one Agent
 * per driver, and refs carry across its calls.
 */
export declare class Agent {
  /** `settleMs`, `image`, `grid`, `cols` (all optional). */
  snapshot(options?: { settleMs?: number; image?: boolean; grid?: boolean; cols?: number }): Promise<Snapshot>
  tap(target: string, options?: { expect?: string }): Promise<TapReport>
  /** Steps as an array of action objects; index.js stringifies them for the native side. */
  steps(steps: unknown[]): Promise<StepsReport>
  /** Record the screen (passively, or around `steps`); the report's `record` has the frames and contact sheets. */
  record(options: RecordOptions, steps?: unknown[]): Promise<StepsReport>
}
export type JsAgent = Agent

export declare class Driver {
  /**
   * `pump` is the JS-side drain loop (index.js wraps the host's onEvent with it).
   * The call argument is a token; the real events come back through `take_event`.
   * Resolves once the driver is live (or after the bounded wait); the wait runs on the
   * libuv threadpool, so the JS thread stays free meanwhile.
   */
  static start(opts: StartOpts, pump: (arg: boolean) => unknown): Promise<Driver>
  get udid(): string
  get state(): string
  /**
   * Drain one parked event (FIFO first, then the latest frame); null when empty.
   * The Buffer for a frame is created here, on the JS thread.
   */
  takeEvent(): JsEvent | null
  /** The JS pump finished: re-arm and, if newer events parked, pump again immediately. */
  pumpDone(): void
  pointer(phaseStr: string, x: number, y: number, mods: JsModifiers): void
  scroll(phaseStr: string, x: number, y: number, dx: number, dy: number): void
  tap(x: number, y: number): void
  /** Two taps 100 ms apart. Resolves after the second. */
  doubleTap(x: number, y: number): Promise<void>
  /** Press at (x, y), hold for `hold_ms` (default 800), then lift — iOS's long press. */
  longPress(x: number, y: number, holdMs?: number | undefined | null): Promise<void>
  /**
   * Drag between two points, each 0..1 in the box: down, hold `hold_ms` (default 800),
   * move over `move_ms` (default 500), hold `destination_hold_ms` (default 0), lift.
   */
  drag(x1: number, y1: number, x2: number, y2: number, holdMs?: number | undefined | null, moveMs?: number | undefined | null, destinationHoldMs?: number | undefined | null): Promise<void>
  /** The agent layer for this driver (one per driver: refs carry across calls). */
  agent(): Agent
  accessibility(): Promise<string>
  touch2(phaseStr: string, x1: number, y1: number, x2: number, y2: number): void
  screenPoints(): JsSize | null
  key(name: string): boolean
  keyCombo(keys: Array<string>): boolean
  clipboardRead(): Promise<string>
  clipboardWrite(text: string): Promise<void>
  openUrl(url: string): Promise<void>
  launch(bundleId: string): Promise<void>
  terminate(bundleId: string): Promise<void>
  setLocation(lat: number, lon: number): Promise<void>
  setAppearance(appearance: string): Promise<void>
  grantPermission(service: string, bundleId: string): Promise<void>
  revokePermission(service: string, bundleId: string): Promise<void>
  resetPermission(service: string, bundleId: string): Promise<void>
  setContentSize(size: string): Promise<void>
  statusBarOverride(): Promise<void>
  statusBarClear(): Promise<void>
  pushNotification(bundleId: string, payloadPath: string): Promise<void>
  resetKeychain(): Promise<void>
  apps(): Promise<Array<App>>
  /** Terminates the app and deletes its data container's contents. Destructive. */
  clearAppData(bundleId: string): Promise<void>
  text(s: string): void
  button(name: string, presses?: number | undefined | null, holdSecs?: number | undefined | null): void
  stats(): JsStats
  /**
   * Stop the driver. Idempotent; after it returns, no further events reach JS and the
   * event loop is not kept alive by this driver.
   */
  stop(): void
}

/** A named grid over a screen: rows lettered down (A, B, …), columns numbered across. */
export declare class Grid {
  constructor(screenWidth: number, screenHeight: number, cols?: number | undefined | null)
  get rows(): number
  get cols(): number
  /** Name of the cell at `row`, `col` (from 0), e.g. (2, 3) → "C4". */
  name(row: number, col: number): string
  /** Centre of a named cell in 0..1 of the screen, or null outside the grid. */
  center(name: string): JsPoint | null
  /** A named cell as {x, y, width, height} in 0..1 of the screen, or null. */
  rect(name: string): JsRect | null
  /** Name of the cell containing (x, y) in 0..1 of the screen. */
  at(x: number, y: number): string
}
export type JsGrid = Grid

export interface App {
  bundleId: string
  name: string
  appType: string
}

export declare function boot(udid: string): void

export interface Device {
  udid: string
  name: string
  state: string
  deviceType: string
  runtime: string
}

/** One drained event; a frame carries the BGRA bytes (a Buffer, built on this thread). */
export interface JsEvent {
  type: string
  state?: string
  text?: string
  width?: number
  height?: number
  sourceWidth?: number
  sourceHeight?: number
  seq?: number
  data?: Buffer
}

export interface JsModifiers {
  alt: boolean
  shift: boolean
  ctrl: boolean
  meta: boolean
}

export interface JsPoint {
  x: number
  y: number
}

export interface JsRect {
  x: number
  y: number
  width: number
  height: number
}

export interface JsSize {
  width: number
  height: number
}

export interface JsSnapshot {
  /** The snapshot without its image, as JSON (index.js parses it). */
  json: string
  image?: Buffer
}

export interface JsStats {
  fps: number
  activeFps: number
  idleFps: number
  lastResponseMs?: number
  maxActiveGapMs: number
  frames: number
}

export declare function listApps(udid: string): Array<App>

export declare function listDevices(): Array<Device>

export declare function shutdown(udid: string): void

export interface StartOpts {
  udid?: string
  fps?: number
  scale?: number
  helperDir?: string
}
