/**
 * Measures the client's real display refresh rate.
 *
 * The host can only avoid judder if it knows how often the client can present.
 * Streaming 60 FPS to a 120 Hz panel is wasteful, and streaming 120 FPS to a
 * 60 Hz panel makes the client drop every second frame, which reads as stutter
 * even though both sides report a healthy frame rate.
 *
 * `requestAnimationFrame` cadence is the refresh rate the WebView actually
 * composites at, which is what matters here.
 */

const COMMON_RATES_HZ = [24, 30, 48, 50, 60, 75, 90, 100, 120, 144, 165, 180, 240];

/**
 * Samples `requestAnimationFrame` for a few frames and returns the detected
 * refresh rate, or `0` when it cannot be measured (hidden document, no rAF).
 */
export async function measureRefreshHz(samples = 12, maxWaitMs = 300): Promise<number> {
  if (typeof requestAnimationFrame !== "function" || typeof performance === "undefined") {
    return 0;
  }
  const stamps: number[] = [];
  const started = performance.now();
  await new Promise<void>((resolve) => {
    let done = false;
    const finish = () => {
      if (done) return;
      done = true;
      resolve();
    };
    const tick = (now: number) => {
      stamps.push(now);
      if (stamps.length >= samples || now - started >= maxWaitMs) {
        finish();
        return;
      }
      requestAnimationFrame(tick);
    };
    requestAnimationFrame(tick);
    // A hidden document produces no callbacks at all.
    window.setTimeout(finish, maxWaitMs + 50);
  });

  if (stamps.length < 4) {
    return 0;
  }
  const deltas: number[] = [];
  for (let i = 1; i < stamps.length; i++) {
    const delta = stamps[i] - stamps[i - 1];
    if (delta > 0.5 && delta < 100) deltas.push(delta);
  }
  if (deltas.length < 3) {
    return 0;
  }
  deltas.sort((a, b) => a - b);
  const median = deltas[deltas.length >> 1];
  if (!(median > 0)) {
    return 0;
  }
  const hz = 1000 / median;
  let best = 0;
  let bestError = Infinity;
  for (const candidate of COMMON_RATES_HZ) {
    const error = Math.abs(candidate - hz);
    if (error < bestError) {
      bestError = error;
      best = candidate;
    }
  }
  // Only snap when the measurement is close to a standard rate.
  const snapped = bestError <= Math.max(1.5, best * 0.04) ? best : Math.round(hz);
  const clamped = Math.min(360, Math.max(24, snapped));
  // A throttled or occluded window reports a very low cadence. Treat that as
  // "unknown" rather than letting it cap the host's frame rate.
  return clamped >= 48 ? clamped : 0;
}
