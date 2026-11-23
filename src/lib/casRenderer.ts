/**
 * Client-side reconstruction: upscale + contrast-adaptive sharpening.
 *
 * The decoded frame is uploaded as a texture and drawn into a canvas whose
 * backing store matches the size the video occupies on screen, so the GPU does
 * the scaling once — with its bilinear filter — and the sharpening kernel then
 * runs over the *displayed* pixels. Sharpening before a resample (the previous
 * behaviour: canvas at stream size, then the browser stretched it) always ends
 * up soft, because the resample throws away exactly the detail that was just
 * added.
 *
 * The kernel itself is the FidelityFX CAS 3x3 contrast-adaptive filter with the
 * reference amplitude limiter (`min(mn, 2 - mx) / (mx - mn)`): edges gain
 * detail, flat areas and already-clipped highlights do not ring. Sharpness 0.4
 * maps to a maximum neighbour weight of `0.4 * 0.25 = 0.1` per tap (centre
 * weight 1.4), the classic "0.4 CAS" setting.
 *
 * The renderer is optional: if WebGL2 or VideoFrame texture upload is missing
 * the caller falls back to the 2D canvas path.
 */

const VERTEX_SRC = `#version 300 es
in vec2 a_pos;
out vec2 v_uv;
void main() {
  // WebGL texture row 0 is the top of the uploaded VideoFrame, while clip
  // space y = -1 is the *bottom* of the viewport. Flipping V here keeps the
  // decoded picture upright; without it the stream renders upside down.
  v_uv = vec2(a_pos.x * 0.5 + 0.5, 0.5 - a_pos.y * 0.5);
  gl_Position = vec4(a_pos, 0.0, 1.0);
}`;

const FRAGMENT_SRC = `#version 300 es
precision highp float;
in vec2 v_uv;
out vec4 out_color;
uniform sampler2D u_tex;
uniform vec2 u_out_texel;
uniform float u_sharpness;

void main() {
  vec3 c = texture(u_tex, v_uv).rgb;
  if (u_sharpness <= 0.0) {
    out_color = vec4(c, 1.0);
    return;
  }
  // Sample offsets are one destination pixel apart. The hardware already
  // upscaled the source with bilinear filtering, so this kernel sharpens the
  // image the display actually shows.
  vec2 d = u_out_texel;
  vec3 n = texture(u_tex, v_uv + vec2(0.0, -d.y)).rgb;
  vec3 s = texture(u_tex, v_uv + vec2(0.0, d.y)).rgb;
  vec3 w = texture(u_tex, v_uv + vec2(-d.x, 0.0)).rgb;
  vec3 e = texture(u_tex, v_uv + vec2(d.x, 0.0)).rgb;
  vec3 nw = texture(u_tex, v_uv + vec2(-d.x, -d.y)).rgb;
  vec3 ne = texture(u_tex, v_uv + vec2(d.x, -d.y)).rgb;
  vec3 sw = texture(u_tex, v_uv + vec2(-d.x, d.y)).rgb;
  vec3 se = texture(u_tex, v_uv + vec2(d.x, d.y)).rgb;

  vec3 mn = min(min(min(nw, ne), min(sw, se)), min(min(n, s), min(w, e)));
  vec3 mx = max(max(max(nw, ne), max(sw, se)), max(max(n, s), max(w, e)));

  // Contrast-adaptive limiter: full strength on real edges, zero on flat
  // areas and on values already at black/white.
  vec3 amplitude = clamp(min(mn, vec3(2.0) - mx) / max(mx - mn, vec3(1e-5)), 0.0, 1.0);
  vec3 k = amplitude * (u_sharpness * 0.25);

  vec3 sharpened = c * (1.0 + 4.0 * k) - (n + s + w + e) * k;
  out_color = vec4(clamp(sharpened, 0.0, 1.0), 1.0);
}`;

export class CasRenderer {
  private readonly gl: WebGL2RenderingContext;
  private readonly program: WebGLProgram;
  private readonly texture: WebGLTexture;
  private readonly outTexel: WebGLUniformLocation | null;
  private readonly sharpness: WebGLUniformLocation | null;
  private width = 0;
  private height = 0;
  private viewportWidth = 0;
  private viewportHeight = 0;

  /** Returns null when WebGL2 is unavailable, so the caller can fall back. */
  static create(canvas: HTMLCanvasElement): CasRenderer | null {
    const gl = canvas.getContext("webgl2", {
      alpha: false,
      antialias: false,
      depth: false,
      stencil: false,
      desynchronized: true,
      powerPreference: "high-performance",
      preserveDrawingBuffer: false,
    });
    if (!gl) return null;
    try {
      return new CasRenderer(gl);
    } catch {
      return null;
    }
  }

  private constructor(gl: WebGL2RenderingContext) {
    this.gl = gl;
    const vertex = CasRenderer.compile(gl, gl.VERTEX_SHADER, VERTEX_SRC);
    const fragment = CasRenderer.compile(gl, gl.FRAGMENT_SHADER, FRAGMENT_SRC);
    const program = gl.createProgram();
    if (!vertex || !fragment || !program) throw new Error("CAS program failed");
    gl.attachShader(program, vertex);
    gl.attachShader(program, fragment);
    gl.linkProgram(program);
    if (!gl.getProgramParameter(program, gl.LINK_STATUS)) {
      throw new Error(gl.getProgramInfoLog(program) ?? "CAS link failed");
    }
    this.program = program;

    const vao = gl.createVertexArray();
    gl.bindVertexArray(vao);
    const buffer = gl.createBuffer();
    gl.bindBuffer(gl.ARRAY_BUFFER, buffer);
    // Full-screen triangle strip.
    gl.bufferData(
      gl.ARRAY_BUFFER,
      new Float32Array([-1, -1, 1, -1, -1, 1, 1, 1]),
      gl.STATIC_DRAW,
    );
    const pos = gl.getAttribLocation(program, "a_pos");
    gl.enableVertexAttribArray(pos);
    gl.vertexAttribPointer(pos, 2, gl.FLOAT, false, 0, 0);

    const texture = gl.createTexture();
    if (!texture) throw new Error("CAS texture failed");
    this.texture = texture;
    gl.bindTexture(gl.TEXTURE_2D, texture);
    gl.texParameteri(gl.TEXTURE_2D, gl.TEXTURE_MIN_FILTER, gl.LINEAR);
    gl.texParameteri(gl.TEXTURE_2D, gl.TEXTURE_MAG_FILTER, gl.LINEAR);
    gl.texParameteri(gl.TEXTURE_2D, gl.TEXTURE_WRAP_S, gl.CLAMP_TO_EDGE);
    gl.texParameteri(gl.TEXTURE_2D, gl.TEXTURE_WRAP_T, gl.CLAMP_TO_EDGE);

    gl.useProgram(program);
    this.outTexel = gl.getUniformLocation(program, "u_out_texel");
    this.sharpness = gl.getUniformLocation(program, "u_sharpness");
    gl.uniform1i(gl.getUniformLocation(program, "u_tex"), 0);
  }

  private static compile(
    gl: WebGL2RenderingContext,
    type: number,
    source: string,
  ): WebGLShader | null {
    const shader = gl.createShader(type);
    if (!shader) return null;
    gl.shaderSource(shader, source);
    gl.compileShader(shader);
    if (!gl.getShaderParameter(shader, gl.COMPILE_STATUS)) {
      gl.deleteShader(shader);
      return null;
    }
    return shader;
  }

  /**
   * Uploads the decoded frame and draws it sharpened over the whole canvas,
   * upscaling from the frame size to the canvas backing size.
   * Returns false when the frame could not be uploaded (caller falls back).
   */
  draw(frame: VideoFrame, sharpness: number): boolean {
    const gl = this.gl;
    if (gl.isContextLost()) return false;

    gl.bindTexture(gl.TEXTURE_2D, this.texture);
    // Chromium uploads VideoFrame directly, RGBA8, no CPU copy.
    try {
      if (
        this.width !== frame.displayWidth ||
        this.height !== frame.displayHeight
      ) {
        gl.texImage2D(
          gl.TEXTURE_2D,
          0,
          gl.RGBA,
          gl.RGBA,
          gl.UNSIGNED_BYTE,
          frame,
        );
        this.width = frame.displayWidth;
        this.height = frame.displayHeight;
      } else {
        gl.texSubImage2D(
          gl.TEXTURE_2D,
          0,
          0,
          0,
          gl.RGBA,
          gl.UNSIGNED_BYTE,
          frame,
        );
      }
    } catch {
      return false;
    }

    // The canvas backing store normally stays fixed for many frames. Avoid
    // reissuing viewport state and uniform updates when the compositor has not
    // resized it; on WebViews this otherwise shows up as measurable driver
    // overhead at 60 FPS.
    const viewportWidth = gl.drawingBufferWidth;
    const viewportHeight = gl.drawingBufferHeight;
    gl.useProgram(this.program);
    if (this.viewportWidth !== viewportWidth || this.viewportHeight !== viewportHeight) {
      this.viewportWidth = viewportWidth;
      this.viewportHeight = viewportHeight;
      gl.viewport(0, 0, viewportWidth, viewportHeight);
      gl.uniform2f(
        this.outTexel,
        1 / Math.max(1, viewportWidth),
        1 / Math.max(1, viewportHeight),
      );
    }
    gl.uniform1f(this.sharpness, sharpness);
    gl.drawArrays(gl.TRIANGLE_STRIP, 0, 4);
    // Chromium already submits at the compositor boundary. Forcing a flush
    // here serializes CPU and GPU work and can reduce 60 FPS throughput.
    return true;
  }
}
