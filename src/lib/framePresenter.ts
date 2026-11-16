/**
 * Vsync-paced frame presentation with "latest frame wins" semantics.
 *
 * Decoding is not presentation. Feeding every decoded frame straight to the
 * canvas means the compositor can be handed two frames for one refresh, or a
 * frame arrives just after the deadline and waits a whole refresh cycle. Both
 * show up as judder that looks exactly like input lag even when the encode is
 * healthy and the reported FPS is fine.
 *
 * The presenter also owns the canvas backing store: it is sized to the pixel
 * area the video occupies on screen (device pixels, capped at 2x the decoded
 * frame), so the frame is resampled exactly once, by our shader, at display
 * resolution. Leaving the canvas at stream size and letting the browser stretch
 * it produced a second, softer resample.
 *
 * So:
 *
 * * every canvas update lands on a display refresh (no mid-frame presents),
 * * a decoded frame superseded before it reaches the display is discarded
 *   instead of queued, so display latency cannot grow,
 * * the upscale happens once, at display resolution, followed by sharpening,
 *   and
 * * when the client falls behind, the backlog shrinks by itself: the newest
 *   frame is always the one shown.
 */

import { CasRenderer } from "./casRenderer";

export interface FramePresenterCallbacks {
  /** The decoded stream resolution (not the canvas backing size). */
  onSize?: (width: number, height: number) => void;
  /** A frame reached the display. */
  onDrawn?: () => void;
}

/** Presentation statistics collected between two `takeWindow()` calls. */
export interface PresentWindow {
  /** Median interval between presented frames, 0 when not measured yet. */
  intervalMs: number;
  /** 95th percentile absolute deviation from that median, in ms. */
  jitterMs: number;
  /** Frames decoded but replaced before a refresh could show them. */
  superseded: number;
  /** Interval samples in the window. */
  samples: number;
}

type CanvasSource = () => HTMLCanvasElement | null;

/** Cap the backing store at 2x the decoded frame in each axis. */
const MAX_UPSCALE = 2;
/** Re-check the layout at most this often, to avoid layout thrash per frame. */
const LAYOUT_INTERVAL_MS = 250;
/** Keep about three seconds of presentation intervals at 60 Hz. */
const INTERVAL_WINDOW = 180;

export class FramePresenter {
  private pending: VideoFrame | null = null;
  private scheduled = 0;
  private renderer: CasRenderer | null = null;
  private rendererTried = false;
  private context2d: CanvasRenderingContext2D | null = null;
  private disposed = false;
  private frameWidth = 0;
  private frameHeight = 0;
  private backingFrameWidth = 0;
  private backingFrameHeight = 0;
  private lastLayout = 0;
  private intervals: number[] = [];
  private lastPresentAt = 0;
  private supersededFrames = 0;

  constructor(
    private readonly canvas: CanvasSource,
    private readonly fallback: CanvasSource,
    private readonly sharpness: () => number,
    private readonly callbacks: FramePresenterCallbacks = {},
  ) {}

  /** Takes ownership of `frame`; it is closed by the presenter. */
  submit(frame: VideoFrame): void {
    if (this.disposed) {
      frame.close();
      return;
    }
    // Newest wins. An undrawn frame is already out of date once a newer one
    // exists, and drawing it would only add latency.
    if (this.pending) {
      this.supersededFrames++;
      this.pending.close();
    }
    this.pending = frame;
    if (this.scheduled === 0) {
      this.scheduled = requestAnimationFrame(this.present);
    }
  }

  /**
   * Returns the presentation window accumulated since the previous call.
   *
   * `intervalMs` is what the viewer actually put on the display; `jitterMs`
   * describes how far those intervals strayed from their median. Together they
   * separate "the stream is running" from "the stream is smooth".
   */
  takeWindow(): PresentWindow {
    const samples = this.intervals.length;
    let intervalMs = 0;
    let jitterMs = 0;
    if (samples > 0) {
      const sorted = [...this.intervals].sort((a, b) => a - b);
      intervalMs = sorted[sorted.length >> 1];
      const deviations = sorted
        .map((value) => Math.abs(value - intervalMs))
        .sort((a, b) => a - b);
      jitterMs = deviations[Math.min(deviations.length - 1, Math.floor(deviations.length * 0.95))];
    }
    const window: PresentWindow = {
      intervalMs,
      jitterMs,
      superseded: this.supersededFrames,
      samples,
    };
    this.intervals = [];
    this.supersededFrames = 0;
    return window;
  }

  /** Drops the undrawn frame, e.g. while the viewer is hidden. */
  discard(): void {
    if (this.scheduled !== 0) {
      cancelAnimationFrame(this.scheduled);
      this.scheduled = 0;
    }
    this.pending?.close();
    this.pending = null;
    // A hidden window produces no refreshes; do not count the gap as a
    // presented-frame interval.
    this.lastPresentAt = 0;
  }

  dispose(): void {
    this.disposed = true;
    this.discard();
  }

  private readonly present = (): void => {
    this.scheduled = 0;
    const now = performance.now();
    const frame = this.pending;
    this.pending = null;
    if (!frame) {
      return;
    }
    try {
      if (!this.disposed && this.draw(frame)) {
        if (this.lastPresentAt > 0) {
          this.intervals.push(now - this.lastPresentAt);
          if (this.intervals.length > INTERVAL_WINDOW) this.intervals.shift();
        }
        this.lastPresentAt = now;
        this.callbacks.onDrawn?.();
      }
    } finally {
      frame.close();
    }
  };

  private draw(frame: VideoFrame): boolean {
    const canvas = this.canvas();
    if (!canvas) {
      return false;
    }
    if (this.frameWidth !== frame.displayWidth || this.frameHeight !== frame.displayHeight) {
      this.frameWidth = frame.displayWidth;
      this.frameHeight = frame.displayHeight;
      this.callbacks.onSize?.(frame.displayWidth, frame.displayHeight);
    }
    this.fitBacking(canvas, frame.displayWidth, frame.displayHeight);

    if (!this.rendererTried) {
      this.rendererTried = true;
      this.renderer = CasRenderer.create(canvas);
    }
    if (this.renderer?.draw(frame, this.effectiveSharpness(canvas))) {
      // If the 2D fallback was used earlier (WebGL context lost), it sits on
      // top of this canvas: leaving it visible would freeze the picture under
      // a stale frame.
      const fallback = this.fallback();
      if (fallback && !fallback.hidden) fallback.hidden = true;
      return true;
    }
    // WebGL2 or VideoFrame upload is unavailable: fall back to a 2D canvas.
    this.renderer = null;
    const fallback = this.fallback();
    if (!fallback) {
      return false;
    }
    // `desynchronized` is ignored (or rejected) by some WebViews; retry with a
    // plain 2D context before giving up on presenting the frame.
    this.context2d ??=
      fallback.getContext("2d", { alpha: false, desynchronized: true }) ??
      fallback.getContext("2d");
    const context = this.context2d;
    if (!context) {
      return false;
    }
    if (fallback.width !== canvas.width || fallback.height !== canvas.height) {
      fallback.width = canvas.width;
      fallback.height = canvas.height;
    }
    try {
      context.drawImage(frame, 0, 0, fallback.width, fallback.height);
    } catch {
      return false;
    }
    fallback.hidden = false;
    return true;
  }

  /**
   * Sharpening strength for the resample actually being performed.
   *
   * Contrast-adaptive sharpening is a reconstruction filter: it recovers detail
   * that an upscale lost. Applied at full strength to a *downscaled* picture — a
   * 1080p stream in a 700 px phone box, or a 4K host on a 1080p screen — it
   * instead adds ringing around every one-pixel text edge, which reads as
   * "over-processed" next to a clean reference.
   *
   * Full strength at 1:1 and above, easing off as the frame is decimated.
   */
  private effectiveSharpness(canvas: HTMLCanvasElement): number {
    const requested = this.sharpness();
    if (requested <= 0 || this.frameWidth <= 0) {
      return 0;
    }
    const scale = canvas.width / this.frameWidth;
    if (!Number.isFinite(scale) || scale <= 0) {
      return requested;
    }
    return requested * Math.min(1, 0.6 + 0.4 * scale);
  }

  /**
   * Sizes the canvas backing store to the video's on-screen pixel area.
   *
   * The element keeps its aspect ratio (the backing store always matches the
   * decoded frame's aspect), so the CSS box and the backing store describe the
   * same rectangle and nothing is resampled a second time by the browser.
   */
  private fitBacking(canvas: HTMLCanvasElement, width: number, height: number): void {
    const now = performance.now();
    if (
      this.backingFrameWidth === width &&
      this.backingFrameHeight === height &&
      now - this.lastLayout < LAYOUT_INTERVAL_MS
    ) {
      return;
    }
    this.lastLayout = now;
    this.backingFrameWidth = width;
    this.backingFrameHeight = height;

    let targetW = width;
    let targetH = height;
    const rect = canvas.getBoundingClientRect();
    if (rect.width >= 2 && rect.height >= 2) {
      const dpr = Math.min(Math.max(window.devicePixelRatio || 1, 1), 2);
      const scale = Math.min(rect.width / width, rect.height / height);
      if (scale > 0) {
        targetW = Math.round(width * scale * dpr);
        targetH = Math.round(height * scale * dpr);
      }
    }
    targetW = Math.max(2, Math.min(targetW, width * MAX_UPSCALE));
    targetH = Math.max(2, Math.min(targetH, height * MAX_UPSCALE));
    if (canvas.width !== targetW || canvas.height !== targetH) {
      canvas.width = targetW;
      canvas.height = targetH;
    }
  }
}
