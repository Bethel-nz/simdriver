'use strict';
// Draws frames and turns DOM input into driver calls. BgraCanvas and attachInput follow
// the planned `simdriver-web` API (Task 12) and move into packages/web when it lands.

/** Draws tightly packed BGRA frames with WebGL1: upload as RGBA, swizzle in the shader. */
class BgraCanvas {
  constructor(canvas) {
    this.canvas = canvas;
    const gl = canvas.getContext('webgl', { antialias: false, alpha: false });
    if (!gl) throw new Error('WebGL unavailable');
    this.gl = gl;
    const shader = (type, src) => {
      const s = gl.createShader(type);
      gl.shaderSource(s, src);
      gl.compileShader(s);
      if (!gl.getShaderParameter(s, gl.COMPILE_STATUS)) throw new Error(gl.getShaderInfoLog(s));
      return s;
    };
    const prog = gl.createProgram();
    gl.attachShader(prog, shader(gl.VERTEX_SHADER,
      'attribute vec2 p; varying vec2 uv;' +
      'void main() { uv = vec2((p.x + 1.0) / 2.0, (1.0 - p.y) / 2.0); gl_Position = vec4(p, 0.0, 1.0); }'));
    gl.attachShader(prog, shader(gl.FRAGMENT_SHADER,
      'precision mediump float; varying vec2 uv; uniform sampler2D t;' +
      'void main() { gl_FragColor = vec4(texture2D(t, uv).bgr, 1.0); }'));
    gl.linkProgram(prog);
    gl.useProgram(prog);
    gl.bindBuffer(gl.ARRAY_BUFFER, gl.createBuffer());
    gl.bufferData(gl.ARRAY_BUFFER, new Float32Array([-1, -1, 1, -1, -1, 1, 1, 1]), gl.STATIC_DRAW);
    const loc = gl.getAttribLocation(prog, 'p');
    gl.enableVertexAttribArray(loc);
    gl.vertexAttribPointer(loc, 2, gl.FLOAT, false, 0, 0);
    this.tex = gl.createTexture();
    gl.bindTexture(gl.TEXTURE_2D, this.tex);
    for (const [k, v] of [
      [gl.TEXTURE_MIN_FILTER, gl.LINEAR], [gl.TEXTURE_MAG_FILTER, gl.LINEAR],
      [gl.TEXTURE_WRAP_S, gl.CLAMP_TO_EDGE], [gl.TEXTURE_WRAP_T, gl.CLAMP_TO_EDGE],
    ]) gl.texParameteri(gl.TEXTURE_2D, k, v);
  }

  draw({ width, height, data }) {
    const { gl, canvas } = this;
    if (canvas.width !== width || canvas.height !== height) {
      canvas.width = width;
      canvas.height = height;
    }
    gl.viewport(0, 0, width, height);
    const bytes = data instanceof Uint8Array ? data : new Uint8Array(data);
    gl.texImage2D(gl.TEXTURE_2D, 0, gl.RGBA, width, height, 0, gl.RGBA, gl.UNSIGNED_BYTE, bytes);
    gl.drawArrays(gl.TRIANGLE_STRIP, 0, 4);
  }

  dispose() {
    this.gl.deleteTexture(this.tex);
  }
}

/** Forward pointer, wheel and keys on `el` to `sink`, in 0..1 coordinates of `el`. */
function attachInput(el, sink) {
  const at = (e) => {
    const r = el.getBoundingClientRect();
    return [(e.clientX - r.left) / r.width, (e.clientY - r.top) / r.height]; // not clamped: the core clamps
  };
  const mods = (e) => ({ alt: e.altKey, shift: e.shiftKey, ctrl: e.ctrlKey, meta: e.metaKey });
  let down = false;
  let lastWheel = 0;

  const onDown = (e) => {
    if (e.button !== 0) return;
    down = true;
    el.setPointerCapture(e.pointerId);
    sink.pointer('down', ...at(e), mods(e));
  };
  const onMove = (e) => down && sink.pointer('move', ...at(e), mods(e));
  const onUp = (e) => {
    if (!down) return;
    down = false;
    sink.pointer('up', ...at(e), mods(e));
  };
  const onWheel = (e) => {
    e.preventDefault();
    const r = el.getBoundingClientRect();
    const [x, y] = at(e);
    // The DOM has no scroll phases: a pause of 150 ms starts a new pan (the core lifts
    // the old one after the same idle time).
    if (e.timeStamp - lastWheel > 150) sink.scroll('began', x, y, 0, 0);
    lastWheel = e.timeStamp;
    // DOM deltas point opposite to the fingers; the core moves the finger with them.
    sink.scroll('changed', x, y, -e.deltaX / r.width, -e.deltaY / r.height);
  };
  const onKey = (e) => {
    if (e.metaKey || e.ctrlKey) return; // leave app shortcuts alone
    e.preventDefault();
    if (e.key.length === 1) sink.text(e.key);
    else sink.key(e.key);
  };

  el.addEventListener('pointerdown', onDown);
  el.addEventListener('pointermove', onMove);
  el.addEventListener('pointerup', onUp);
  el.addEventListener('pointercancel', onUp);
  el.addEventListener('wheel', onWheel, { passive: false });
  el.addEventListener('keydown', onKey);
  return () => {
    el.removeEventListener('pointerdown', onDown);
    el.removeEventListener('pointermove', onMove);
    el.removeEventListener('pointerup', onUp);
    el.removeEventListener('pointercancel', onUp);
    el.removeEventListener('wheel', onWheel);
    el.removeEventListener('keydown', onKey);
  };
}

const canvas = document.getElementById('screen');
const screen = new BgraCanvas(canvas);
const state = document.getElementById('state');
const sd = window.simdriver;

let selftestPending = true;
sd.onFrame((frame) => {
  screen.draw(frame);
  if (selftestPending) {
    selftestPending = false;
    // Report where the canvas is once this frame is on screen (selftest only listens).
    requestAnimationFrame(() => requestAnimationFrame(() => {
      const r = canvas.getBoundingClientRect();
      sd.selftestDrawn({ x: r.x, y: r.y, width: r.width, height: r.height });
    }));
  }
});
sd.onState((s) => (state.textContent = s));

// Input is measured against the canvas (the device screen); it is focusable, so it gets keys.
attachInput(canvas, sd);
canvas.addEventListener('pointerdown', () => canvas.focus());
document.getElementById('home').onclick = () => sd.button('home');
document.getElementById('lock').onclick = () => sd.button('lock');
canvas.focus();
