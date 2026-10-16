import { createPointRenderer } from './graphics.js';

// Deterministic 10,000-point dataset; no network traffic or server actor is involved.
function makePoints(count) {
  const result = new Float32Array(count * 2);
  let seed = 0x6f21ab3d;
  for (let i = 0; i < result.length; i++) {
    seed = (Math.imul(seed, 1664525) + 1013904223) >>> 0;
    result[i] = ((seed / 0x100000000) * 1.9) - 0.95;
  }
  return result;
}

const canvas = document.getElementById('plot');
const slider = document.getElementById('density');
const status = document.getElementById('backend');
const points = makePoints(10_000);
let renderer;
let unsubscribe = () => {};
try {
  const signal = window.nulang?.signal('pointCount');
  if (!signal?.subscribe) throw new Error('Nulang client signal runtime is unavailable');
  renderer = createPointRenderer(canvas);
  status.textContent = 'Rendering with ' + renderer.backend;
  const updatePoints = value => {
    const count = Math.max(1_000, Math.min(10_000, Number(value)));
    renderer.setPoints(points.subarray(0, count * 2));
  };
  unsubscribe = signal.subscribe(updatePoints);
  updatePoints(signal.get());
  slider.addEventListener('input', () => signal.set(Number(slider.value)));
} catch (error) {
  status.textContent = 'Graphics unavailable: ' + error.message;
}
window.addEventListener('pagehide', () => {
  unsubscribe();
  renderer?.dispose();
}, { once: true });
