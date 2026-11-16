import { useEffect, useRef, type PointerEvent } from "react";
import type { RemoteInputPayload, TouchAction } from "../types";

export type StreamPoint = { xNorm: number; yNorm: number };
type Touch = StreamPoint & { x: number; y: number };

const DRAG_THRESHOLD = 6;
const RIGHT_CLICK_DELAY = 500;
const DOUBLE_TAP_DELAY = 300;

export function useDesktopTouch(
  enabled: boolean,
  mode: "trackpad" | "direct",
  locate: (canvas: HTMLCanvasElement, x: number, y: number, clamp?: boolean) => StreamPoint | null,
  dispatch: (input: RemoteInputPayload) => void,
  nextTapId: () => number,
  cancelFocus: () => void,
) {
  const touches = useRef(new Map<number, Touch>());
  const start = useRef<Touch | null>(null);
  const drag = useRef<StreamPoint | null>(null);
  const scrolling = useRef(false);
  const scrollX = useRef(0);
  const scrollY = useRef(0);
  const hold = useRef<number | undefined>(undefined);
  const frame = useRef<number | undefined>(undefined);
  const cursor = useRef<StreamPoint>({ xNorm: 0.5, yNorm: 0.5 });
  const lastTap = useRef(0);
  const moved = useRef(false);
  const dragGesture = useRef(false);
  const cursorMotion = useRef(false);

  const send = (point: StreamPoint, action: TouchAction, tapId = 0) =>
    dispatch({ kind: "touch_pointer", xNorm: point.xNorm, yNorm: point.yNorm, action, tapId });

  useEffect(() => {
    const cancel = () => {
      window.clearTimeout(hold.current);
      if (frame.current !== undefined) window.cancelAnimationFrame(frame.current);
      frame.current = undefined;
      if (drag.current) {
        dispatch({ kind: "touch_pointer", ...drag.current, action: "up", tapId: 0 });
      }
      drag.current = null;
      start.current = null;
      touches.current.clear();
      scrolling.current = false;
      scrollX.current = 0;
      scrollY.current = 0;
      lastTap.current = 0;
      cursorMotion.current = false;
    };
    window.addEventListener("blur", cancel);
    window.addEventListener("resize", cancel);
    return () => {
      window.removeEventListener("blur", cancel);
      window.removeEventListener("resize", cancel);
      cancel();
    };
  }, [enabled, mode, dispatch]);

  const down = (event: PointerEvent<HTMLCanvasElement>) => {
    if (!enabled || touches.current.size >= 2) return;
    const point = mode === "trackpad" ? cursor.current : locate(event.currentTarget, event.clientX, event.clientY);
    if (!point) return;
    event.preventDefault();
    event.currentTarget.setPointerCapture(event.pointerId);
    cancelFocus();
    const touch = { ...point, x: event.clientX, y: event.clientY };
    touches.current.set(event.pointerId, touch);
    if (touches.current.size === 2) {
      window.clearTimeout(hold.current);
      if (drag.current) send(drag.current, "up");
      drag.current = null;
      start.current = null;
      scrolling.current = true;
      if (mode === "direct") dispatch({ kind: "mouse_move_abs", ...point });
      return;
    }
    moved.current = false;
    dragGesture.current = mode === "direct" || performance.now() - lastTap.current < DOUBLE_TAP_DELAY && lastTap.current > 0;
    start.current = touch;
    hold.current = window.setTimeout(() => {
      if (start.current && !drag.current) {
        send(start.current, "context");
        start.current = null;
      }
    }, RIGHT_CLICK_DELAY);
  };

  const move = (event: PointerEvent<HTMLCanvasElement>) => {
    const previous = touches.current.get(event.pointerId);
    if (!enabled || !previous) return;
    const point = mode === "trackpad" ? cursor.current : locate(event.currentTarget, event.clientX, event.clientY, true);
    if (!point) return;
    const touch = { ...point, x: event.clientX, y: event.clientY };
    touches.current.set(event.pointerId, touch);

    if (scrolling.current && touches.current.size === 2) {
      scrollX.current -= (touch.x - previous.x) * 1.5;
      scrollY.current -= (touch.y - previous.y) * 1.5;
    } else if (mode === "trackpad" && !scrolling.current && !dragGesture.current) {
      const rect = event.currentTarget.getBoundingClientRect();
      const scale = Math.max(1, Math.min(rect.width, rect.height));
      const clamp = (value: number) => Math.max(0, Math.min(1, value));
      if (start.current && Math.hypot(touch.x - start.current.x, touch.y - start.current.y) < DRAG_THRESHOLD) return;
      const origin = start.current ?? previous;
      window.clearTimeout(hold.current);
      moved.current = true;
      start.current = null;
      cursor.current = {
        xNorm: clamp(cursor.current.xNorm + (touch.x - origin.x) / scale),
        yNorm: clamp(cursor.current.yNorm + (touch.y - origin.y) / scale),
      };
      cursorMotion.current = true;
    } else if (start.current || drag.current) {
      if (start.current && !drag.current) {
        if (Math.hypot(touch.x - start.current.x, touch.y - start.current.y) < DRAG_THRESHOLD) return;
        window.clearTimeout(hold.current);
        send(start.current, "down");
        start.current = null;
      }
      if (mode === "trackpad") {
        const scale = Math.max(1, Math.min(event.currentTarget.clientWidth, event.currentTarget.clientHeight));
        cursor.current = {
          xNorm: Math.max(0, Math.min(1, point.xNorm + (touch.x - previous.x) / scale)),
          yNorm: Math.max(0, Math.min(1, point.yNorm + (touch.y - previous.y) / scale)),
        };
        drag.current = cursor.current;
      } else {
        drag.current = point;
      }
    } else {
      return;
    }

    if (frame.current !== undefined) return;
    frame.current = window.requestAnimationFrame(() => {
      frame.current = undefined;
      if (drag.current) send(drag.current, "drag");
      if (cursorMotion.current) {
        cursorMotion.current = false;
        dispatch({ kind: "mouse_move_abs", ...cursor.current });
      }
      const deltaX = Math.trunc(scrollX.current);
      const deltaY = Math.trunc(scrollY.current);
      if (deltaX || deltaY) {
        scrollX.current -= deltaX;
        scrollY.current -= deltaY;
        dispatch({ kind: "mouse_wheel", deltaX, deltaY });
      }
    });
  };

  const end = (event: PointerEvent<HTMLCanvasElement>, cancelled = false) => {
    const previous = touches.current.get(event.pointerId);
    if (!previous) return;
    touches.current.delete(event.pointerId);
    window.clearTimeout(hold.current);
    if (scrolling.current) {
      if (touches.current.size === 0) scrolling.current = false;
      return;
    }
    const point = mode === "trackpad" ? cursor.current : locate(event.currentTarget, event.clientX, event.clientY, true) ?? previous;
    if (drag.current) {
      send(cancelled ? drag.current : point, "up");
    } else if (start.current && !moved.current && !cancelled) {
      send(point, "tap", nextTapId());
      lastTap.current = performance.now();
    }
    drag.current = null;
    start.current = null;
  };

  return { down, move, end };
}
