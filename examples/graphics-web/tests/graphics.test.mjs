import test from 'node:test';
import assert from 'node:assert/strict';
import { createPointRenderer } from '../static/graphics.js';

function fakeCanvas({ webgl = false } = {}) {
  const calls = { clear: 0, arcs: 0, drawArrays: 0, buffers: 0, deletedBuffers: 0 };
  const ctx2d = {
    setTransform() {}, clearRect() { calls.clear++; }, beginPath() {},
    arc() { calls.arcs++; }, fill() {},
    set fillStyle(_) {}
  };
  const gl = {
    VERTEX_SHADER: 1, FRAGMENT_SHADER: 2, COMPILE_STATUS: 3, LINK_STATUS: 4,
    ARRAY_BUFFER: 5, STATIC_DRAW: 6, FLOAT: 7, POINTS: 8, COLOR_BUFFER_BIT: 9,
    createShader() { return {}; }, shaderSource() {}, compileShader() {},
    getShaderParameter() { return true; }, deleteShader() {},
    createProgram() { return {}; }, attachShader() {}, linkProgram() {},
    getProgramParameter() { return true; }, deleteProgram() {}, useProgram() {},
    getAttribLocation() { return 0; }, enableVertexAttribArray() {}, vertexAttribPointer() {},
    getUniformLocation() { return {}; }, uniform1f() {},
    createBuffer() { calls.buffers++; return {}; }, bindBuffer() {}, bufferData() {},
    deleteBuffer() { calls.deletedBuffers++; }, viewport() {}, clearColor() {},
    clear() {}, drawArrays(_, __, count) { calls.drawArrays++; calls.lastCount = count; }
  };
  const listeners = new Map();
  const canvas = {
    width: 0, height: 0, clientWidth: 200, clientHeight: 100,
    getContext(type) { return type === 'webgl2' && webgl ? gl : type === '2d' && !webgl ? ctx2d : null; },
    addEventListener(name, fn) { listeners.set(name, fn); },
    removeEventListener(name) { listeners.delete(name); }
  };
  return { canvas, calls, listeners };
}

function clock() {
  const frames = new Map();
  let nextId = 0;
  return {
    requestFrame(fn) { const id = ++nextId; frames.set(id, fn); return id; },
    cancelFrame(id) { frames.delete(id); },
    flush() { const pending = [...frames.values()]; frames.clear(); pending.forEach(fn => fn()); },
    get size() { return frames.size; }
  };
}

test('Canvas2D fallback batches point updates into one frame and cleans up', () => {
  const { canvas, calls } = fakeCanvas();
  const raf = clock();
  const renderer = createPointRenderer(canvas, { requestFrame: raf.requestFrame, cancelFrame: raf.cancelFrame, pixelRatio: 2 });
  assert.equal(renderer.backend, 'canvas2d');
  renderer.setPoints(new Float32Array([0, 0]));
  renderer.setPoints(new Float32Array([0, 0, 0.5, -0.5]));
  assert.equal(raf.size, 1);
  raf.flush();
  assert.equal(canvas.width, 400);
  assert.equal(canvas.height, 200);
  assert.equal(calls.clear, 1);
  assert.equal(calls.arcs, 2);
  renderer.dispose();
  assert.throws(() => renderer.setPoints(new Float32Array(0)), /disposed/);
});

test('WebGL2 creates buffers, batches redraws and releases resources', () => {
  const { canvas, calls } = fakeCanvas({ webgl: true });
  const raf = clock();
  const renderer = createPointRenderer(canvas, { requestFrame: raf.requestFrame, cancelFrame: raf.cancelFrame });
  assert.equal(renderer.backend, 'webgl2');
  renderer.setPoints(new Float32Array([0, 0, 1, -1]));
  renderer.setPoints(new Float32Array([0, 0, 1, -1, -1, 1]));
  raf.flush();
  assert.equal(calls.drawArrays, 1);
  assert.equal(calls.lastCount, 3);
  renderer.dispose();
  assert.equal(calls.buffers, calls.deletedBuffers);
});

test('WebGL context loss suspends drawing and restoration recreates resources', () => {
  const { canvas, calls, listeners } = fakeCanvas({ webgl: true });
  const raf = clock();
  const renderer = createPointRenderer(canvas, { requestFrame: raf.requestFrame, cancelFrame: raf.cancelFrame });
  renderer.setPoints(new Float32Array([0, 0]));
  let prevented = false;
  listeners.get('webglcontextlost')({ preventDefault() { prevented = true; } });
  assert.equal(prevented, true);
  assert.equal(raf.size, 0);
  listeners.get('webglcontextrestored')();
  assert.equal(calls.buffers, 2);
  raf.flush();
  assert.equal(calls.drawArrays, 1);
  renderer.dispose();
  assert.equal(listeners.size, 0);
});

test('rejects invalid geometry instead of passing it to graphics APIs', () => {
  const { canvas } = fakeCanvas();
  const raf = clock();
  const renderer = createPointRenderer(canvas, { requestFrame: raf.requestFrame, cancelFrame: raf.cancelFrame });
  assert.throws(() => renderer.setPoints([0, 1]), /Float32Array/);
  assert.throws(() => renderer.setPoints(new Float32Array([0])), /pairs/);
  assert.throws(() => renderer.setPoints(new Float32Array([Infinity, 0])), /finite/);
  renderer.dispose();
});
