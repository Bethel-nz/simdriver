// pip-mini on simdriver — the original "embed a headless device in GPUix" proof, with
// every pixel and every input going through one `Driver`.
//
//  box    = <img> fed by ref.setImagePixels(bgra)               (the "view")
//  div    = wrapper with onLayout(rect) + pointer/key/scroll     (the "hotspot")
//  driver = simdriver: frames from the device's own IOSurface, touches through
//           dtuhidd, edge gestures / pinch / pan / scroll-as-pan decided in the core
//
// The host only normalizes coordinates to the box (0..1) and passes modifiers.

import { useEffect, useRef, useState } from 'react'
import { render, type EventPayload } from '@gpuix/react'
import type { ImgInstance } from '@gpuix/react'
import { Driver, boot, listDevices, shutdown, type Device, type DriverState } from 'simdriver'

const BOX_W = 310
const FPS_LABEL_MS = 250 // readout refresh; per-frame setState would re-render the whole app

// 1×1 transparent PNG — holds the pane until the first frame lands.
const BLANK_PNG =
  'data:image/png;base64,iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mNkYAAAAAYAAjCB0C8AAAAASUVORK5CYII='

type Btn = 'home' | 'lock' | 'siri' | 'side'
const SCROLL_PHASE = { started: 'began', moved: 'changed', ended: 'ended' } as const

function App() {
  const [devices, setDevices] = useState<Device[]>([])
  const [sel, setSel] = useState<Device | null>(null)
  const [state, setState] = useState<DriverState | null>(null)
  const [fps, setFps] = useState<number | null>(null)
  const [dims, setDims] = useState<{ width: number; height: number; source: string } | null>(null)
  const [booting, setBooting] = useState(false)
  const [log, setLog] = useState<string[]>(['pick a device · boot · go live · tap the box'])
  const [lastPt, setLastPt] = useState('—')
  const img = useRef<ImgInstance>(null)
  const driver = useRef<Driver | null>(null)
  // Bumped by every start/stop: a start that resolves after a newer one is stopped.
  const startGen = useRef(0)
  const selRef = useRef<Device | null>(null)
  const rectRef = useRef({ x: 0, y: 0, width: 0, height: 0 })
  const dimsRef = useRef<{ width: number; height: number } | null>(null)
  const down = useRef(false)

  selRef.current = sel

  function add(line: string) {
    setLog(l => [`${new Date().toLocaleTimeString()}·  ${line}`, ...l].slice(0, 26))
  }

  useEffect(() => {
    refresh()
    // fps readout on a timer: frames only arrive when the sim presents, so a still
    // screen reads as idle instead of freezing at the last burst's rate.
    const id = setInterval(() => {
      const d = driver.current
      const next = d ? d.stats().fps : null
      setFps(prev => (prev === next ? prev : next))
    }, FPS_LABEL_MS)
    return () => {
      clearInterval(id)
      driver.current?.stop() // hot reload / unmount: the next start gets a clean device
    }
  }, [])

  function refresh() {
    try {
      const all = listDevices()
      setDevices(all)
      const booted = all.filter(d => d.state === 'Booted')
      // PIP_UDID pins a device (e.g. a dedicated test sim when several are booted).
      const pinned = Bun.env.PIP_UDID ? all.find(d => d.udid === Bun.env.PIP_UDID) : undefined
      const pick = pinned ?? booted[0] ?? all.find(d => d.deviceType.includes('iPhone')) ?? all[0] ?? null
      setSel(pick)
      add(`devices: ${all.length} seen (${booted.length} booted) → ${pick ? pick.name : 'none'}`)
      if (pick?.state === 'Booted' && !driver.current) startLive(pick)
    } catch (e) {
      add(`list failed: ${String(e).slice(0, 140)}`)
    }
  }

  async function bootSelected() {
    const d = selRef.current
    if (!d) return
    setBooting(true)
    add(`booting ${d.name} (headless — no Device Hub window involved)…`)
    // Yield one tick so the "booting…" label paints before the blocking call.
    await new Promise(r => setTimeout(r, 0))
    try {
      boot(d.udid)
      add('boot: ok')
      refresh()
    } catch (e) {
      add(`boot failed: ${String(e).slice(0, 160)}`)
    } finally {
      setBooting(false)
    }
  }

  // Hot path at 60 fps: React state changes only when the frame size does.
  function paint(width: number, height: number, data: Uint8Array, sw: number, sh: number) {
    img.current?.setImagePixels(width, height, data, { format: 'bgra' })
    const prev = dimsRef.current
    if (!prev || prev.width !== width || prev.height !== height) {
      dimsRef.current = { width, height }
      setDims({ width, height, source: `${sw}×${sh}` })
      add(`first frame ${sw}×${sh}px (streamed at ${width}×${height})`)
    }
  }

  async function startLive(device?: Device) {
    const d = device ?? selRef.current
    if (!d) return
    stopLive(true)
    const gen = ++startGen.current
    add(`live ● ${d.name}…`)
    setState('starting')
    try {
      const started = await Driver.start({ udid: d.udid }, e => {
        if (gen !== startGen.current) return
        if (e.type === 'frame') paint(e.width, e.height, e.data, e.sourceWidth, e.sourceHeight)
        else if (e.type === 'state') {
          setState(e.state)
          add(`state: ${e.state}`)
        } else add(e.text)
      })
      if (gen !== startGen.current) return started.stop()
      driver.current = started
      setState(started.state)
    } catch (e) {
      if (gen !== startGen.current) return
      setState(null)
      add(`start failed: ${String(e).slice(0, 160)}`)
    }
  }

  function stopLive(silent = false) {
    startGen.current++
    if (!driver.current) return
    driver.current.stop()
    driver.current = null
    dimsRef.current = null
    setState(null)
    setDims(null)
    if (!silent) add('live ○')
  }

  // ─── event capture → 0..1 of the box ──────────────────────────────────────
  // GPUix hands us window coords; onLayout gave us the pane rect. Not clamped:
  // the driver clamps, and a drag past the edge (pointer capture) keeps streaming.
  function norm(e: EventPayload) {
    const r = rectRef.current
    if (!r.width || !r.height) return null
    return { x: ((e.x ?? 0) - r.x) / r.width, y: ((e.y ?? 0) - r.y) / r.height }
  }

  const mods = (e: EventPayload) => ({ alt: !!e.modifiers?.alt, shift: !!e.modifiers?.shift })

  function sendText(text: string) {
    if (!driver.current) return add('text needs live mode')
    driver.current.text(text)
    add(`text → "${text.slice(0, 28)}"`)
  }

  function sendButton(b: Btn) {
    if (!driver.current) return add('buttons need live mode')
    driver.current.button(b)
    add(`button ${b}`)
  }

  const live = driver.current !== null

  const btn = {
    display: 'flex',
    alignItems: 'center',
    paddingTop: 7,
    paddingBottom: 7,
    paddingLeft: 12,
    paddingRight: 12,
    borderRadius: 9,
    backgroundColor: '#1d1f26',
    cursor: 'pointer' as const,
    hover: { backgroundColor: '#2a2d3a' },
    active: { backgroundColor: '#3a3f52' },
  } as const

  return (
    <div style={{ display: 'flex', height: '100%', gap: 18, padding: 16, backgroundColor: '#121216' }}>
      {/* ─── the device pane: wrapper div (rect + input), img fills it ─── */}
      <div
        tabIndex={0}
        onLayout={e => {
          rectRef.current = { x: e.x ?? 0, y: e.y ?? 0, width: e.width ?? 0, height: e.height ?? 0 }
        }}
        onMouseDown={e => {
          const p = norm(e)
          if (!p || !driver.current) return
          down.current = true
          driver.current.pointer('down', p.x, p.y, mods(e))
        }}
        onMouseMove={e => {
          const p = norm(e)
          if (down.current && p) driver.current?.pointer('move', p.x, p.y, mods(e))
        }}
        onMouseUp={e => {
          const p = norm(e)
          if (!down.current || !p) return
          down.current = false
          driver.current?.pointer('up', p.x, p.y, mods(e))
          setLastPt(`${(p.x * 100).toFixed(0)}% × ${(p.y * 100).toFixed(0)}%`)
        }}
        onScroll={e => {
          // Trackpad scroll = a live one-finger pan. A notched mouse wheel has no
          // phases to lift on, so it stays unmapped; momentum has no phase either.
          const phase = SCROLL_PHASE[e.touchPhase as keyof typeof SCROLL_PHASE]
          const p = norm(e)
          const r = rectRef.current
          if (!e.precise || !phase || !p || !driver.current) return
          driver.current.scroll(phase, p.x, p.y, (e.deltaX ?? 0) / r.width, (e.deltaY ?? 0) / r.height)
        }}
        onKeyDown={e => {
          if (!driver.current) return
          // Named keys first (enter, escape, arrows…): they can also carry a keyChar.
          if (driver.current.key(e.key ?? '')) return
          const ch = e.keyChar ?? ''
          if (ch.length === 1) driver.current.text(ch)
        }}
        style={{
          display: 'flex',
          flexDirection: 'column',
          width: BOX_W,
          height: 'auto' as const,
          aspectRatio: dims ? dims.width / dims.height : 0.466,
          // The row's default align-items: stretch would override aspectRatio and
          // stretch the device to the window height.
          alignSelf: 'flex-start',
          flexShrink: 0,
          borderRadius: 26,
          backgroundColor: '#050507',
          borderWidth: 1.5,
          borderColor: '#2c2c34',
          padding: 2,
          focusVisible: { outlineWidth: 2, outlineColor: '#4a5dff' },
          cursor: 'pointer',
        }}
      >
        <img ref={img} src={BLANK_PNG} objectFit="fill" style={{ width: '100%', height: '100%', borderRadius: 24 }} />
      </div>

      {/* ─── the app side: controls, state, log ─── */}
      <div style={{ display: 'flex', flexDirection: 'column', flexGrow: 1, gap: 12, minWidth: 0 }}>
        <text style={{ color: '#e8e8f0', fontSize: 15 }}>{'pip-mini on simdriver — box ▸ device round-trip'}</text>

        <div style={{ display: 'flex', gap: 8, alignItems: 'center', flexWrap: 'wrap' }}>
          <div style={btn} onClick={refresh}><text style={{ color: '#c9cbe0' }}>{'refresh devices'}</text></div>
          <div style={btn} onClick={() => void bootSelected()}><text style={{ color: '#c9cbe0' }}>{booting ? 'booting…' : 'boot headless'}</text></div>
          {sel?.state === 'Booted'
            ? (live
                ? <div style={btn} onClick={() => stopLive()}><text style={{ color: '#ff8a8a' }}>{'live ● stop'}</text></div>
                : <div style={btn} onClick={() => startLive()}><text style={{ color: '#8affa0' }}>{'live ● start'}</text></div>)
            : <text style={{ color: '#7a7a8a', fontSize: 12 }}>{'boot a device to go live'}</text>}
          <div style={btn} onClick={() => {
            const d = selRef.current
            if (!d) return
            stopLive(true)
            try {
              shutdown(d.udid)
              add('shutdown ok')
              refresh()
            } catch (err) {
              add(`shutdown: ${String(err).slice(0, 90)}`)
            }
          }}><text style={{ color: '#c9cbe0' }}>{'shutdown'}</text></div>
        </div>

        {/* device picker */}
        <div style={{ display: 'flex', flexDirection: 'column', gap: 2, maxHeight: 148, overflowY: 'scroll', padding: 6, backgroundColor: '#17181d', borderRadius: 10 }}>
          {devices.map(d => (
            <div
              key={d.udid}
              onClick={() => { setSel(d); add(`selected ${d.name} (${d.state})`) }}
              style={{
                display: 'flex',
                paddingTop: 5,
                paddingBottom: 5,
                paddingLeft: 8,
                paddingRight: 8,
                borderRadius: 7,
                cursor: 'pointer',
                backgroundColor: sel?.udid === d.udid ? '#262a38' : 'transparent',
                hover: { backgroundColor: '#20222c' },
              }}
            >
              <text style={{ color: sel?.udid === d.udid ? '#aeb8ff' : '#9a9ab0', fontSize: 12 }}>
                {`${d.state === 'Booted' ? '● ' : '○ '}${d.name} · ${d.runtime.replace(/.*SimRuntime\./, '').replace('iOS-', 'iOS ').replace(/-/g, '.')} · ${d.state}`}
              </text>
            </div>
          ))}
        </div>

        {/* status strip */}
        <div style={{ display: 'flex', gap: 16, flexWrap: 'wrap' }}>
          <text style={{ color: '#8a8aa0', fontSize: 12 }}>{sel ? `device: ${sel.name} · ${sel.udid.slice(0, 8)}` : 'device: —'}</text>
          <text style={{ color: '#8a8aa0', fontSize: 12 }}>{`state: ${state ?? '—'}`}</text>
          <text style={{ color: '#8a8aa0', fontSize: 12 }}>{dims ? `frame: ${dims.source}px → ${dims.width}×${dims.height}` : 'frame: —'}</text>
          <text style={{ color: '#8a8aa0', fontSize: 12 }}>{fps === null ? '' : fps === 0 ? 'idle · sim not presenting' : `${Math.round(fps)} fps`}</text>
          <text style={{ color: '#8a8aa0', fontSize: 12 }}>{`last touch at: ${lastPt}`}</text>
        </div>

        {/* hardware buttons */}
        <div style={{ display: 'flex', gap: 8, flexWrap: 'wrap' }}>
          {(['home', 'lock', 'siri', 'side'] as const).map(b => (
            <div key={b} style={btn} onClick={() => sendButton(b)}>
              <text style={{ color: '#c9cbe0', fontSize: 12 }}>{b}</text>
            </div>
          ))}
          <div style={btn} onClick={() => sendText('hello pip-mini')}>
            <text style={{ color: '#c9cbe0', fontSize: 12 }}>{'send text'}</text>
          </div>
        </div>

        {/* log */}
        <div style={{ display: 'flex', flexDirection: 'column', gap: 3, flexGrow: 1, padding: 8, backgroundColor: '#17181d', borderRadius: 10, overflowY: 'scroll' }}>
          {log.map((line, i) => (
            <text key={`${i}-${line.slice(0, 12)}`} style={{ color: i === 0 ? '#dcdcea' : '#8b8ca6', fontSize: 11 }}>{line}</text>
          ))}
        </div>

        <text style={{ color: '#5c5c70', fontSize: 10 }}>
          {'simdriver: frames from the device’s own IOSurface · touches through dtuhidd · gestures decided in the driver · nothing here screen-records'}
        </text>
      </div>
    </div>
  )
}

render(<App />, {
  title: 'pip-mini',
  appName: 'pip-mini',
  width: 1040,
  height: 780,
  windowBackground: 'opaque',
})
