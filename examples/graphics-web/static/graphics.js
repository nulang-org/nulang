// Browser-only graphics interop prototype. Not part of the Nulang compiler/runtime.
// Coordinates are normalized device coordinates: [-1, 1] for both axes.
const VERTEX = `#version 300 es
in vec2 a_position;
uniform float u_pointSize;
void main() {
  gl_Position = vec4(a_position, 0.0, 1.0);
  gl_PointSize = u_pointSize;
}`;
const FRAGMENT = `#version 300 es
precision mediump float;
out vec4 outColor;
void main() {
  vec2 xy = gl_PointCoord - vec2(0.5);
  if (dot(xy, xy) > 0.25) discard;
  outColor = vec4(0.12, 0.42, 0.93, 1.0);
}`;

function makeProgram(gl) {
  const shaders = [];
  const compile = (type, source) => {
    const shader = gl.createShader(type);
    if (!shader) throw new Error('unable to allocate WebGL shader');
    shaders.push(shader);
    gl.shaderSource(shader, source);
    gl.compileShader(shader);
    if (!gl.getShaderParameter(shader, gl.COMPILE_STATUS)) {
      throw new Error('WebGL shader compilation failed: ' + gl.getShaderInfoLog(shader));
    }
    return shader;
  };
  let program;
  try {
    const vertex = compile(gl.VERTEX_SHADER, VERTEX);
    const fragment = compile(gl.FRAGMENT_SHADER, FRAGMENT);
    program = gl.createProgram();
    if (!program) throw new Error('unable to allocate WebGL program');
    gl.attachShader(program, vertex);
    gl.attachShader(program, fragment);
    gl.linkProgram(program);
    if (!gl.getProgramParameter(program, gl.LINK_STATUS)) {
      throw new Error('WebGL program linking failed: ' + gl.getProgramInfoLog(program));
    }
    return program;
  } catch (error) {
    if (program) gl.deleteProgram(program);
    throw error;
  } finally {
    shaders.forEach(shader => gl.deleteShader(shader));
  }
}

export function createPointRenderer(canvas, options = {}) {
  const {
    pixelRatio = Math.min(globalThis.devicePixelRatio || 1, 2),
    pointSize = 3,
    requestFrame = callback => globalThis.requestAnimationFrame(callback),
    cancelFrame = id => globalThis.cancelAnimationFrame(id)
  } = options;
  if (!(pointSize > 0 && Number.isFinite(pointSize))) throw new RangeError('pointSize must be positive and finite');
  if (!(pixelRatio > 0 && Number.isFinite(pixelRatio))) throw new RangeError('pixelRatio must be positive and finite');
  const gl = canvas.getContext('webgl2', { alpha: false, antialias: false });
  const ctx = gl ? null : canvas.getContext('2d');
  if (!gl && !ctx) throw new Error('Neither WebGL2 nor Canvas2D is supported');
  let program = null;
  let buffer = null;
  let attribute = -1;
  let uniform = null;
  let points = new Float32Array(0);
  let bufferDirty = true;
  let scheduled = null;
  let lost = false;
  let disposed = false;

  function initializeGpu() {
    program = makeProgram(gl);
    try {
      buffer = gl.createBuffer();
      if (!buffer) throw new Error('unable to allocate WebGL buffer');
      attribute = gl.getAttribLocation(program, 'a_position');
      uniform = gl.getUniformLocation(program, 'u_pointSize');
      if (attribute < 0 || uniform == null) throw new Error('WebGL shader inputs are unavailable');
      bufferDirty = true;
    } catch (error) {
      if (buffer) gl.deleteBuffer(buffer);
      gl.deleteProgram(program);
      buffer = null;
      program = null;
      throw error;
    }
  }

  function measure() {
    const width = Math.max(1, Math.round(canvas.clientWidth * pixelRatio));
    const height = Math.max(1, Math.round(canvas.clientHeight * pixelRatio));
    if (canvas.width !== width || canvas.height !== height) {
      canvas.width = width;
      canvas.height = height;
    }
  }

  function draw() {
    scheduled = null;
    if (disposed || lost) return;
    measure();
    if (gl) {
      gl.viewport(0, 0, canvas.width, canvas.height);
      gl.clearColor(1, 1, 1, 1);
      gl.clear(gl.COLOR_BUFFER_BIT);
      gl.useProgram(program);
      gl.bindBuffer(gl.ARRAY_BUFFER, buffer);
      if (bufferDirty) {
        gl.bufferData(gl.ARRAY_BUFFER, points, gl.STATIC_DRAW);
        bufferDirty = false;
      }
      gl.enableVertexAttribArray(attribute);
      gl.vertexAttribPointer(attribute, 2, gl.FLOAT, false, 0, 0);
      gl.uniform1f(uniform, pointSize * pixelRatio);
      gl.drawArrays(gl.POINTS, 0, points.length / 2);
    } else {
      ctx.setTransform(pixelRatio, 0, 0, pixelRatio, 0, 0);
      ctx.clearRect(0, 0, canvas.clientWidth, canvas.clientHeight);
      ctx.fillStyle = '#1f6bed';
      for (let i = 0; i < points.length; i += 2) {
        ctx.beginPath();
        ctx.arc((points[i] + 1) * canvas.clientWidth / 2,
          (1 - points[i + 1]) * canvas.clientHeight / 2, pointSize / 2, 0, Math.PI * 2);
        ctx.fill();
      }
    }
  }

  function scheduleDraw() {
    if (!disposed && !lost && scheduled === null) scheduled = requestFrame(draw);
  }

  function onContextLost(event) {
    event.preventDefault(); // Ask the browser to attempt WebGL restoration.
    lost = true;
    if (scheduled !== null) cancelFrame(scheduled);
    scheduled = null;
    // GL objects are invalid after context loss; don't delete them now.
    program = null;
    buffer = null;
  }
  function onContextRestored() {
    if (disposed) return;
    initializeGpu();
    lost = false;
    scheduleDraw();
  }

  if (gl) {
    initializeGpu();
    canvas.addEventListener('webglcontextlost', onContextLost);
    canvas.addEventListener('webglcontextrestored', onContextRestored);
  }
  const observer = typeof globalThis.ResizeObserver === 'function'
    ? new globalThis.ResizeObserver(scheduleDraw) : null;
  if (observer) observer.observe(canvas);

  return {
    backend: gl ? 'webgl2' : 'canvas2d',
    setPoints(nextPoints) {
      if (disposed) throw new Error('point renderer has been disposed');
      if (!(nextPoints instanceof Float32Array)) throw new TypeError('points must be a Float32Array');
      if (nextPoints.length % 2 !== 0) throw new RangeError('points must be x/y pairs');
      for (const coordinate of nextPoints) {
        if (!Number.isFinite(coordinate)) throw new RangeError('points must contain finite coordinates');
      }
      points = nextPoints;
      bufferDirty = true;
      scheduleDraw();
    },
    resize() {
      if (disposed) throw new Error('point renderer has been disposed');
      scheduleDraw();
    },
    dispose() {
      if (disposed) return;
      disposed = true;
      if (scheduled !== null) cancelFrame(scheduled);
      if (observer) observer.disconnect();
      if (gl) {
        canvas.removeEventListener('webglcontextlost', onContextLost);
        canvas.removeEventListener('webglcontextrestored', onContextRestored);
        if (!lost) {
          if (buffer) gl.deleteBuffer(buffer);
          if (program) gl.deleteProgram(program);
        }
      }
      scheduled = null;
      buffer = null;
      program = null;
      points = new Float32Array(0);
    }
  };
}
