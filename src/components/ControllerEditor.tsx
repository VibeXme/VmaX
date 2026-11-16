import { useRef, useState, type PointerEvent } from "react";
import { Check, ChevronLeft, Gamepad2, Move, Redo2, RotateCcw, Scaling, Undo2 } from "lucide-react";
import {
  BUTTONS, CONTROLS, controlName, defaultLayout, isStick,
  type Button, type Control, type ControllerLayout,
} from "../config/touchController";
import { TouchGamepad } from "./TouchGamepad";

type History = { past: ControllerLayout[]; current: ControllerLayout; future: ControllerLayout[] };
type Gesture = {
  pointer: number; id: Control; before: ControllerLayout;
  x: number; y: number; scale: number; width: number; height: number;
  radiusX: number; radiusY: number; startX: number; startY: number;
};

export function ControllerEditor({ initial, opacity, size, onSave, onClose }: {
  initial: ControllerLayout; opacity: number; size: number;
  onSave: (layout: ControllerLayout) => boolean; onClose: () => void;
}) {
  const [history, setHistory] = useState<History>({ past: [], current: initial, future: [] });
  const [preview, setPreview] = useState<ControllerLayout | null>(null);
  const [selected, setSelected] = useState<Control>("leftStick");
  const [mode, setMode] = useState<"move" | "size">("move");
  const [error, setError] = useState("");
  const gesture = useRef<Gesture | null>(null);
  const layout = preview ?? history.current;
  const clamp = (value: number, min: number, max: number) => Math.max(min, Math.min(max, value));

  function commit(current: ControllerLayout) {
    setHistory((old) => ({ past: [...old.past, old.current].slice(-40), current, future: [] }));
  }

  function step(forward: boolean) {
    setHistory((old) => {
      if (forward) {
        const [current, ...future] = old.future;
        return current ? { past: [...old.past, old.current], current, future } : old;
      }
      const current = old.past[old.past.length - 1];
      return current ? { past: old.past.slice(0, -1), current, future: [old.current, ...old.future] } : old;
    });
  }

  function down(event: PointerEvent<HTMLElement>, id: Control) {
    if (gesture.current || event.button !== 0) return;
    const target = event.currentTarget;
    const bounds = target.getBoundingClientRect();
    const area = target.parentElement?.getBoundingClientRect();
    if (!area?.width || !area.height) return;
    setSelected(id);
    target.setPointerCapture(event.pointerId);
    gesture.current = {
      pointer: event.pointerId, id, before: history.current,
      x: (bounds.left + bounds.width / 2 - area.left) / area.width * 100,
      y: (bounds.top + bounds.height / 2 - area.top) / area.height * 100,
      radiusX: bounds.width / area.width * 50, radiusY: bounds.height / area.height * 50,
      scale: layout.placements[id]?.scale ?? 100, width: area.width, height: area.height,
      startX: event.clientX, startY: event.clientY,
    };
  }

  function dragged(event: PointerEvent<HTMLElement>) {
    const active = gesture.current;
    if (!active || event.pointerId !== active.pointer) return null;
    const dx = event.clientX - active.startX;
    const dy = event.clientY - active.startY;
    if (Math.hypot(dx, dy) < 3) return null;
    const { id, before } = active;
    const change = mode === "move"
      ? { x: clamp(active.x + dx / active.width * 100, active.radiusX, 100 - active.radiusX),
          y: clamp(active.y + dy / active.height * 100, active.radiusY, 100 - active.radiusY) }
      : { scale: clamp(Math.round(active.scale + dx - dy), 60, 150) };
    return { ...before, placements: { ...before.placements, [id]: { ...before.placements[id], ...change } } };
  }

  function end(event: PointerEvent<HTMLElement>, cancel = false) {
    if (gesture.current?.pointer !== event.pointerId) return;
    const next = cancel ? null : dragged(event);
    gesture.current = null;
    setPreview(null);
    if (next) commit(next);
  }

  const placement = layout.placements[selected];
  function setScale(scale: number) {
    commit({ ...layout, placements: { ...layout.placements, [selected]: { ...placement, scale } } });
  }

  return (
    <section className="controller-editor" data-stream-controls aria-label="Controller editor"
      onKeyDown={(event) => {
        if (event.key === "Escape") {
          event.stopPropagation();
          onClose();
        }
      }}>
      <TouchGamepad layout={layout} size={size} opacity={opacity} editor={{
        selected, down, move: (event) => {
          if (gesture.current?.pointer === event.pointerId) setPreview(dragged(event));
        }, end, select: setSelected,
      }} />
      <header className="controller-editor-header">
        <button type="button" aria-label="Cancel editing" onClick={onClose}><ChevronLeft size={20} /></button>
        <h2><Gamepad2 size={18} /> <span>Controller Editor</span></h2>
        <div className="controller-editor-actions">
          <button type="button" aria-label="Undo" disabled={!history.past.length} onClick={() => step(false)}><Undo2 size={18} /></button>
          <button type="button" aria-label="Redo" disabled={!history.future.length} onClick={() => step(true)}><Redo2 size={18} /></button>
          <select aria-label="Layout preset" value={layout.preset} onChange={(event) => commit({
            ...layout, preset: event.target.value as ControllerLayout["preset"], placements: {},
          })}>
            <option value="default">Default</option><option value="mirrored">Mirrored</option>
          </select>
          <button type="button" className="controller-editor-save" onClick={() => {
            if (!onSave(history.current)) setError("Could not save this layout on this device.");
          }}><Check size={18} /> Save</button>
        </div>
      </header>
      <div className="controller-editor-dock">
        {error && <p role="alert">{error}</p>}
        <div className="controller-editor-row">
          <label className="sr-only" htmlFor="controller-control">Control</label>
          <select id="controller-control" value={selected} onChange={(event) => setSelected(event.target.value as Control)}>
            {CONTROLS.map((id) => <option key={id} value={id}>{controlName(id)}</option>)}
          </select>
          {isStick(selected) ? (
            <label><input type="checkbox" checked={layout.swapSticks}
              onChange={(event) => commit({ ...layout, swapSticks: event.target.checked })} /> Swap sticks</label>
          ) : (
            <label>Maps to <select value={layout.bindings[selected] ?? selected} onChange={(event) => commit({
              ...layout, bindings: { ...layout.bindings, [selected]: event.target.value as Button },
            })}>
              {BUTTONS.map((id) => <option key={id} value={id}>{controlName(id)}</option>)}
            </select></label>
          )}
          <button type="button" onClick={() => commit({
            ...layout, placements: { ...layout.placements, [selected]: { ...placement, hidden: !placement?.hidden } },
          })}>{placement?.hidden ? "Show" : "Hide"}</button>
        </div>
        <div className="controller-editor-row">
          <button type="button" aria-pressed={mode === "move"} onClick={() => setMode("move")}><Move size={16} /> Move</button>
          <button type="button" aria-pressed={mode === "size"} onClick={() => setMode("size")}><Scaling size={16} /> Size</button>
          {mode === "size" && <label>Scale <input type="range" min={60} max={150} step={5} value={placement?.scale ?? 100}
            onChange={(event) => setScale(Number(event.target.value))} /></label>}
          <label>Opacity <input type="range" min={20} max={100} step={5} value={layout.opacity ?? opacity}
            onChange={(event) => commit({ ...layout, opacity: Number(event.target.value) })} />
            <span>{layout.opacity ?? opacity}%</span>
          </label>
          <button type="button" onClick={() => commit(defaultLayout())}><RotateCcw size={16} /> Reset</button>
        </div>
      </div>
    </section>
  );
}
