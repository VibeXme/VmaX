/**
 * Decoder health: when to heal the reference chain, and when to rebuild the
 * decoder entirely.
 *
 * The two are not the same decision, and conflating them is expensive.
 * `VideoDecoder.reset()` throws away every queued chunk and then refuses to
 * produce a picture until a keyframe arrives, which costs a keyframe-sized
 * burst on the wire (a 1080p60 IDR is several hundred kilobytes) and drops
 * every delta frame in flight. Doing that because three frames were briefly
 * queued turns one Wi-Fi retransmit into a visible freeze followed by a
 * bitrate spike, and the spike makes the next stall more likely — a loop that
 * sustains itself.
 *
 * Almost all backlog is benign. Decoding is faster than the stream rate, so a
 * queue that formed during a burst drains by itself, and `FramePresenter`
 * already throws away decoded frames that a newer one supersedes. The only
 * two situations that cannot recover on their own are:
 *
 * * the decoder is far enough behind that it can no longer be described as
 *   realtime (`BACKLOG_RESET_FRAMES` of 60 Hz input is a fifth of a second),
 *   and
 * * it has stopped producing output altogether while work is queued.
 *
 * Both are measured against *observed progress*, never against queue depth
 * alone, so a decoder that is merely busy is left alone.
 */

/**
 * Queue depth at which the decoder is no longer keeping up with real time.
 * Eight frames is ~133 ms at 60 FPS: past the point where dropping the
 * backlog costs less than showing it.
 */
export const BACKLOG_RESET_FRAMES = 8;

/**
 * A queued decoder that has produced nothing for this long is stuck rather
 * than busy. Hardware decoders answer in 1-3 ms, so a quarter of a second of
 * silence with work outstanding cannot be transient.
 */
export const STALL_RESET_MS = 250;

export type DecodeAction = "ok" | "reset";

export type DecodeReason = "backlog" | "stalled";

export interface DecodeVerdict {
  action: DecodeAction;
  /** Why the decoder had to be rebuilt, for the recovery log. */
  reason: DecodeReason | null;
}

const HEALTHY: DecodeVerdict = { action: "ok", reason: null };

export class DecoderHealth {
  private lastProgressAt: number | null = null;
  private queuedSince: number | null = null;

  /** A frame came out of the decoder, so the queue is moving. */
  noteProgress(nowMs: number): void {
    this.lastProgressAt = nowMs;
  }

  /** The decoder was rebuilt: forget the history of the previous one. */
  reset(): void {
    this.lastProgressAt = null;
    this.queuedSince = null;
  }

  /**
   * Classifies the decoder before a chunk is submitted.
   *
   * `queueDepth` is `VideoDecoder.decodeQueueSize` as of just before the
   * submit. The returned action is advisory: a caller that ignores a `reset`
   * keeps decoding on a chain the decoder has already given up on.
   */
  evaluate(queueDepth: number, nowMs: number): DecodeVerdict {
    if (queueDepth <= 0) {
      this.queuedSince = null;
      return HEALTHY;
    }
    const since = this.queuedSince ?? nowMs;
    this.queuedSince = since;
    if (queueDepth >= BACKLOG_RESET_FRAMES) {
      return { action: "reset", reason: "backlog" };
    }
    // The stall clock starts at whichever is later: the moment work first
    // appeared, or the last time the decoder actually produced something.
    const reference = Math.max(since, this.lastProgressAt ?? since);
    if (nowMs - reference >= STALL_RESET_MS) {
      return { action: "reset", reason: "stalled" };
    }
    return HEALTHY;
  }
}
