# simdriver controls

simdriver exposes low-level simulator control through `Driver` and higher-level, screen-aware actions through `Agent`. The Node package includes both; the Rust crates expose the same core controls, and the C API exposes the core driver controls as `sd_*` functions.

## Agent

Create an `Agent` from a `Driver`. `snapshot()` waits for the screen to settle and returns accessible elements with refs (`e1`, `e2`, …), plus an annotated image when requested. Refs are checked against the current screen before use. Screens without accessible elements can be addressed with the lettered grid (`A1`, `B2`, …). `steps()` runs actions in order and stops at the first failure.

Pass these actions to `Agent.steps()`:

| Action | Description |
| --- | --- |
| `tap` | Tap a ref, visible label, identifier, or grid cell. |
| `doubleTap` | Tap the same validated target twice. |
| `longPress` | Press and hold a target; defaults to 800 ms. |
| `drag` | Drag between two targets, with configurable source hold, movement, and destination hold times. |
| `pinch` | Pinch around a target or screen center; `scale > 1` zooms in and `scale < 1` zooms out. |
| `rotate` | Rotate around a target or screen center by degrees. |
| `pan` | Move two fingers together around a target or screen center. |
| `transform` | Combine two-finger pan, scale, and rotation in one stroke. |
| `fling` | Fast directional swipe with momentum (`up`, `down`, `left`, `right`). |
| `swipe` | Swipe between targets or normalized `[x, y]` points; can repeat up to 10 times. |
| `scroll` | Scroll up, down, left, or right, optionally within a target. Scrolls avoid the detected keyboard. |
| `fill` | Replace a text field’s contents, then report whether the value was verified. `secret: true` redacts it from reports. |
| `type` | Type text into the focused control. Non-ASCII text uses the simulator pasteboard, so the app sees a paste. |
| `key` | Press a named key or key combination, such as `enter` or `cmd+a`. |
| `press` | Press a hardware button: `home`, `lock`, `side`, `siri`, `volume-up`, or `volume-down`. |
| `waitFor` | Wait until visible text contains a string, ignoring case. |
| `openUrl` | Open a URL in the simulator; reports and accepts a matching “Open in …” prompt. |
| `launch` / `terminate` | Launch or terminate an app by bundle ID. |
| `setLocation` | Set simulator latitude and longitude. |
| `appearance` | Set light or dark appearance. |
| `clipboardRead` / `clipboardWrite` | Read or write the simulator pasteboard; writing an empty string clears it. |
| `keyboardDismiss` | Dismiss the on-screen keyboard and verify it is gone. |
| `back` | Navigate with the system edge gesture or an in-app back control. |
| `appSwitcher` | Open the app switcher with a double Home-button press. |
| `screenshotElement` | Return a cropped PNG of a target element. |
| `record` | Record the screen over the steps that follow; first step only. See Record. |
| `alert` | Get, wait for, accept, or dismiss a visible system alert. Dismiss uses known safe button labels only. |

Example:

```ts
const result = await agent.steps([
  { action: 'doubleTap', target: 'Map' },
  { action: 'pinch', target: 'Map', scale: 1.5 },
  { action: 'waitFor', text: 'Done', timeoutMs: 5000 },
])
```

The step report includes completion/failure details, fill verification, clipboard reads, screenshots, alert details, and the latest snapshot.

## Record

Use `record` when the result of a step shows up over time: a transition, a toast, a spinner, a stutter. It records the screen around the steps and returns a contact sheet plus a few timed full-size frames. Read the sheet first, then fetch frames by index.

```ts
const report = await agent.record({ timeMs: 5000 }, [{ action: 'tap', target: 'Save' }]);
const { sheets, frames, ended, settledMs, health } = report.record;
```

`agent.record(options, steps?)` is `agent.steps([{ ...options, action: 'record' }, ...steps])`; with no steps it only watches. CLI: `simdriver agent record [--time 5s] [--idle-ms 400] [--full] [--fps 30] [--max-frames 24] [--out dir] [-- tap <target> | -- steps <json | @file>]`. `record` is valid only as the first step and covers the whole run; anywhere else (or twice) the run fails with `invalid` before any step runs.

| Option | CLI | Default | Limits |
| --- | --- | --- | --- |
| `timeMs` | `--time` (`5s`, `800ms`, bare ms) | 5000 | at most 30000 |
| `idleMs` | `--idle-ms` | 400 | 50 to 5000 |
| `full` | `--full` | false | record the whole time instead of ending when still |
| `fps` | `--fps` | 30 | 25 to 30 |
| `maxFrames` | `--max-frames` | 24 | 2 to 32 |

**When it ends.** After the actions finish, the recording ends once the screen has been still for `idleMs` (`ended: "idle"`). `timeMs` caps it (`"time"`). If the actions are still running at `timeMs`, the recording extends until they finish (`extended: true`), then ends the normal way: still, or `timeMs` after the actions ended. With `full` it records the whole `timeMs`, or until the actions finish if later. A failed step ends it `idleMs` later with `"stepFailed"`; a lost frame stream ends it with `"error"` and `error` set. A recording is never thrown away: failure still returns it. The recorder sees the screen only through arriving frames, so a stream that stalls (the frame helper restarting mid-recording) looks like a still screen: it can end `"idle"` and report `settledMs` as if the screen had settled.

**Sampling limit.** The recorder looks at 25 to 30 frames a second (default 30), so a change shorter than one sample (about 33 ms) can be missed. For a brief flash, hold the state longer (for example a long press) or record more than once.

**Output.** `report.record` has the contact `sheets` (PNG Buffers, usually one, thumbnails 180 px wide), kept `frames` (`index`, `tMs`, `change`, `png`), and the manifest fields `ended`, `extended`, `requestedMs`, `durationMs`, `actionStartMs`, `actionEndMs`, `settledMs` (the last change; `null` means still changing when it ended), `health` (`received`, `sampled`, `skippedByPipeline`, `skippedByRecorder`, `maxGapMs`, `gapsOver34ms`: a stutter is reported, not just seen) and `error`. All times are milliseconds from the baseline frame at `t = 0`. Only changed frames are kept, thinned to `maxFrames`.

**Reading the sheet.** Each tile is labelled `#<index> +<t>ms` and framed by its place relative to the actions: grey before they started, blue while they ran, green after they ended (with no actions, everything after the baseline is green). A legend across the top of each sheet repeats this.

**CLI output.** The manifest is the report's `record` field: `sheetFiles` (first, so the sheet is read first), `frames[].file`, then the fields above. The CLI writes `sheet-N.png`, `frame-<index>-<t>ms.png` and `record.json` (just that `record` object) to `--out` (default `~/.cache/simdriver/records/<udid>-<time>/`) and prints the report with those paths. `simdriver agent steps '[{"action":"record",…},…]' [--out dir]` writes the same files by the same rules. An `--out` that cannot be written is refused before the simulator is touched; if a write still fails, the recording goes to a fresh temporary directory, the report shows those paths, and the error names both (exit 1). It also exits 1 when the recording ended in `error`.

## Driver controls

The table lists Node `Driver` method names. Rust exposes the same controls with snake_case names where available. C exposes low-level driver operations as `sd_*` functions; frames arrive through its callback, and accessibility is returned as JSON.

| Area | Node `Driver` methods | Description |
| --- | --- | --- |
| Pointer and gestures | `pointer`, `scroll`, `tap`, `doubleTap`, `longPress`, `drag`, `touch2` | Send normalized pointer input, scroll, taps, long presses, drags, or exact two-finger phases. |
| Keyboard and buttons | `key`, `keyCombo`, `text`, `button` | Press keys, key combinations, type text, and press hardware buttons. Button presses can repeat. Keys include letters, digits, Enter, Return, Escape, Backspace, Tab, Space, Delete, Home, End, Page Up, Page Down, and arrow keys. Key combinations support `cmd`, `shift`, `alt`, and `ctrl`. |
| Screen and accessibility | `accessibility`, `screenPoints`, `grid` | Read the frontmost app’s accessibility tree, get point dimensions, or map points to named grid cells. Rust also exposes `capture()` for a still frame. Node and C receive frames through the driver event callback. |
| State and events | `state`, `stats`, `takeEvent`, `pumpDone`, `stop` | Track driver state and frame/input statistics, drain Node events, and stop the driver. `start()` streams frame, state, and diagnostic events. |
| Simulator apps | `apps`, `openUrl`, `launch`, `terminate` | List installed apps (including system apps), open a URL, and launch or terminate an app. App entries include `bundleId`, `name`, and `appType`. |
| App data | `clearAppData` | Terminate an app and erase the contents of its simulator data container. This is destructive and runs only when called explicitly. |
| Simulator settings | `setLocation`, `setAppearance`, `grantPermission`, `revokePermission`, `resetPermission`, `setContentSize`, `statusBarOverride`, `statusBarClear`, `pushNotification`, `resetKeychain` | Configure simulator location, appearance, permissions, text size, status bar, notifications, and keychain. |
| Pasteboard | `clipboardRead`, `clipboardWrite` | Read or write text on the simulator pasteboard. |

The Node package also exports `listDevices()`, `listApps(udid)`, `boot(udid)`, and `shutdown(udid)`. Rust exposes the corresponding device functions. C exposes `sd_apps(driver)` for the running driver’s simulator.

## Boundaries

- Biometric controls are not implemented. Device orientation was investigated but is not exposed. Three-or-more-finger gestures are not supported.
- `fill` verifies the resulting accessibility value and can report a mismatch or an unobserved commit. On Xcode 27.1, Unicode text written with `simctl pbcopy` appeared in `simctl pbpaste` but did not paste into either Settings Search or Safari’s address field; `fill` returned `verified: false`. App-originated copy/paste and ASCII HID typing worked. In Safari, send `cmd+l` before the snapshot to expose and focus its address field. Similar simulator pasteboard synchronization failures have been [reported on Apple Developer Forums](https://developer.apple.com/forums/thread/820393) and [here](https://developer.apple.com/forums/thread/820644).
- `clearAppData` deletes saved app data. Use it only when that reset is intended.
