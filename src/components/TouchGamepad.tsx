import { useEffect, useRef, useState, type CSSProperties, type PointerEvent } from "react";
import { invoke } from "@tauri-apps/api/core";
import { Menu, PanelsTopLeft } from "lucide-react";
import {
  BUTTON_LABELS, CONTROLS, controlName, controlPosition, isStick,
  type Button, type Control, type ControllerLayout, type Point, type Stick,
} from "../config/touchController";
import "./TouchGamepad.css";

type TouchReport = { buttons: Button[]; left: Point; right: Point };
type Contact =
  | { button: Button; target: Button }
  | { stick: Stick; target: Stick; origin: Point; radius: number; position: Point };

const CENTER: Point = [0, 0];
const NEUTRAL: TouchReport = { buttons: [], left: CENTER, right: CENTER };
const REFRESH_MS = 250;

function sendReport(input: TouchReport | null) {
  void invoke("set_touch_gamepad", { input }).catch(() => {});
}

type EditHandlers = {
  selected: Control;
  down: (event: PointerEvent<HTMLElement>, id: Control) => void;
  move: (event: PointerEvent<HTMLElement>) => void;
  end: (event: PointerEvent<HTMLElement>, cancel?: boolean) => void;
  select: (id: Control) => void;
};

export function TouchGamepad({ layout, opacity, size, editor }: {
  layout: ControllerLayout; opacity: number; size: number; editor?: EditHandlers;
}) {
  const area = useRef<HTMLDivElement>(null);
  const contacts = useRef(new Map<number, Contact>());
  const motionFrame = useRef<number | null>(null);
  const report = useRef<TouchReport | null>(null);
  const [display, setDisplay] = useState(NEUTRAL);
  const [portrait, setPortrait] = useState(false);
  const editing = Boolean(editor);

  useEffect(() => {
    if (!area.current) return;
    const observer = new ResizeObserver(([entry]) =>
      setPortrait(entry.contentRect.width < entry.contentRect.height));
    observer.observe(area.current);
    return () => observer.disconnect();
  }, []);

  function publish() {
    if (motionFrame.current !== null) {
      cancelAnimationFrame(motionFrame.current);
      motionFrame.current = null;
    }
    const next: TouchReport = { buttons: [], left: CENTER, right: CENTER };
    const visual: TouchReport = { buttons: [], left: CENTER, right: CENTER };
    for (const contact of contacts.current.values()) {
      if ("button" in contact) {
        visual.buttons.push(contact.button);
        if (!next.buttons.includes(contact.target)) next.buttons.push(contact.target);
      } else {
        visual[contact.stick] = contact.position;
        next[contact.target] = contact.position;
      }
    }
    report.current = next;
    setDisplay(visual);
    sendReport(report.current);
  }

  useEffect(() => {
    if (editing) return;
    const activeContacts = contacts.current;
    function clear() {
      if (motionFrame.current !== null) {
        cancelAnimationFrame(motionFrame.current);
        motionFrame.current = null;
      }
      activeContacts.clear();
      if (report.current) {
        report.current = null;
        sendReport(null);
      }
    }
    function reset() {
      clear();
      setDisplay(NEUTRAL);
    }
    function onVisibilityChange() {
      if (document.hidden) reset();
      else if (document.hasFocus()) activate();
    }
    function activate() {
      if (document.hidden || report.current) return;
      report.current = NEUTRAL;
      sendReport(NEUTRAL);
    }
    activate();
    const refresh = window.setInterval(() => {
      if (report.current && !document.hidden) sendReport(report.current);
    }, REFRESH_MS);
    window.addEventListener("blur", reset);
    window.addEventListener("focus", activate);
    window.addEventListener("pagehide", reset);
    window.addEventListener("resize", releaseOnResize);
    document.addEventListener("visibilitychange", onVisibilityChange);
    return () => {
      clearInterval(refresh);
      window.removeEventListener("blur", reset);
      window.removeEventListener("pagehide", reset);
      window.removeEventListener("focus", activate);
      window.removeEventListener("resize", releaseOnResize);
      document.removeEventListener("visibilitychange", onVisibilityChange);
      clear();
    };
    function releaseOnResize() {
      reset();
      if (document.hasFocus()) activate();
    }
  }, [editing]);

  function start(event: PointerEvent<HTMLElement>, contact: Contact, publishNow = true) {
    if (event.button !== 0) return;
    event.preventDefault();
    event.currentTarget.setPointerCapture(event.pointerId);
    contacts.current.set(event.pointerId, contact);
    if (publishNow) publish();
  }

  function startStick(event: PointerEvent<HTMLElement>, stick: Stick) {
    if ([...contacts.current.values()].some((contact) => "stick" in contact && contact.stick === stick)) return;
    const bounds = event.currentTarget.getBoundingClientRect();
    start(event, {
      stick,
      target: layout.swapSticks ? (stick === "left" ? "right" : "left") : stick,
      origin: [bounds.left + bounds.width / 2, bounds.top + bounds.height / 2],
      radius: Math.max(1, bounds.width * 0.38),
      position: CENTER,
    }, false);
    moveStick(event);
    if (contacts.current.has(event.pointerId)) publish();
  }

  function moveStick(event: PointerEvent<HTMLElement>) {
    const contact = contacts.current.get(event.pointerId);
    if (!contact || "button" in contact) return;
    const x = event.clientX - contact.origin[0];
    const y = event.clientY - contact.origin[1];
    const distance = Math.hypot(x, y);
    const deadZone = contact.radius * 0.12;
    if (distance <= deadZone) {
      contact.position = CENTER;
    } else {
      const strength = Math.min(1, (distance - deadZone) / (contact.radius - deadZone));
      contact.position = [x / distance * strength, y / distance * strength];
    }
    if (motionFrame.current === null) motionFrame.current = requestAnimationFrame(publish);
  }

  function end(event: PointerEvent<HTMLElement>) {
    if (contacts.current.delete(event.pointerId)) publish();
  }

  function control(id: Control) {
    const placement = layout.placements[id];
    if (placement?.hidden && !editor) return null;
    const [x, y] = controlPosition(id, layout, portrait);
    const stick = isStick(id);
    const side = id === "leftStick" ? "left" : "right";
    const [stickX, stickY] = display[side];
    const binding = stick ? null : (layout.bindings[id] ?? id);
    const shape = stick ? "stick" : /^(lt|rt|lb|rb)$/.test(id) ? "shoulder" :
      /^(up|down|left|right)$/.test(id) ? "dpad" : /^(view|menu|l3|r3)$/.test(id) ? "small" : "face";
    const label = binding === "view" ? <PanelsTopLeft size="55%" /> :
      binding === "menu" ? <Menu size="55%" /> : binding ? BUTTON_LABELS[binding] : null;
    return (
      <button
        key={id}
        type="button"
        tabIndex={editor ? 0 : -1}
        aria-label={editor || !binding ? controlName(id) : controlName(binding)}
        data-control={id}
        data-shape={shape}
        data-binding={binding}
        data-pressed={!stick && display.buttons.includes(id)}
        data-selected={editor?.selected === id}
        data-hidden={placement?.hidden === true}
        className="touch-gamepad-control"
        style={{ "--control-x": `${x}%`, "--control-y": `${y}%`, "--control-scale": (placement?.scale ?? 100) / 100 } as CSSProperties}
        onPointerDown={(event) => {
          if (editor) editor.down(event, id);
          else if (stick) startStick(event, side);
          else if (binding) start(event, { button: id, target: binding });
        }}
        onPointerMove={editor ? editor.move : stick ? moveStick : undefined}
        onPointerUp={editor ? (event) => editor.end(event) : end}
        onPointerCancel={editor ? (event) => editor.end(event, true) : end}
        onLostPointerCapture={editor ? (event) => editor.end(event, true) : end}
        onClick={editor ? () => editor.select(id) : undefined}
      >
        {stick ? <span className="touch-gamepad-thumb" style={{ transform: `translate(${stickX * 54}%, ${stickY * 54}%)` }} /> : label}
      </button>
    );
  }

  return (
    <div
      ref={area}
      role="group"
      aria-label="Touch gamepad"
      className="touch-gamepad"
      data-editing={Boolean(editor)}
      style={{ "--pad-opacity": (layout.opacity ?? opacity) / 100, "--pad-scale": size / 100 } as CSSProperties}
      onTouchStart={(event) => event.stopPropagation()}
      onContextMenu={(event) => event.preventDefault()}
    >
      {CONTROLS.map(control)}
    </div>
  );
}
