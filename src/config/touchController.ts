export const BUTTON_LABELS = {
  a: "A", b: "B", x: "X", y: "Y", lb: "LB", rb: "RB", lt: "LT", rt: "RT",
  up: "▲", down: "▼", left: "◀", right: "▶", view: "View", menu: "Menu", l3: "L3", r3: "R3",
} as const;

export type Button = keyof typeof BUTTON_LABELS;
export type Stick = "left" | "right";
export type Control = Button | "leftStick" | "rightStick";
export type Point = [number, number];
export type Placement = { x?: number; y?: number; scale?: number; hidden?: boolean };
export type ControllerLayout = {
  preset: "default" | "mirrored";
  opacity?: number;
  placements: Partial<Record<Control, Placement>>;
  bindings: Partial<Record<Button, Button>>;
  swapSticks: boolean;
};

export function defaultLayout(preset: ControllerLayout["preset"] = "default"): ControllerLayout {
  return { preset, placements: {}, bindings: {}, swapSticks: false };
}

export const BUTTONS = Object.keys(BUTTON_LABELS) as Button[];
export const CONTROLS: Control[] = ["leftStick", "rightStick", ...BUTTONS];
export const isStick = (id: Control): id is "leftStick" | "rightStick" =>
  id === "leftStick" || id === "rightStick";

const positions: Record<Control, [Point, Point]> = {
  leftStick: [[12, 76], [18, 69]], rightStick: [[74, 78], [62, 72]],
  lt: [[8, 38], [13, 49]], lb: [[8, 48], [13, 56]], l3: [[13, 58], [31, 59]],
  rt: [[92, 38], [87, 49]], rb: [[92, 48], [87, 56]], r3: [[76, 58], [69, 59]],
  up: [[26, 68], [32, 80]], left: [[22, 77], [23, 86]],
  right: [[30, 77], [41, 86]], down: [[26, 86], [32, 92]],
  y: [[89, 64], [83, 67]], x: [[84, 75], [73, 75]],
  b: [[94, 75], [93, 75]], a: [[89, 86], [83, 83]],
  view: [[46, 87], [50, 91]], menu: [[54, 87], [64, 91]],
};

export function controlName(id: Control): string {
  if (isStick(id)) return id === "leftStick" ? "Left joystick" : "Right joystick";
  if (id === "up" || id === "down" || id === "left" || id === "right") return `D-pad ${id}`;
  return BUTTON_LABELS[id];
}

export function controlPosition(id: Control, layout: ControllerLayout, portrait: boolean): Point {
  const [x, y] = positions[id][portrait ? 1 : 0];
  const placement = layout.placements[id];
  return [placement?.x ?? (layout.preset === "mirrored" ? 100 - x : x), placement?.y ?? y];
}

export const CONTROLLER_LAYOUT_KEY = "vmax.touch-controller.v1";
const bounded = (value: unknown, min: number, max: number) =>
  typeof value === "number" && Number.isFinite(value) ? Math.max(min, Math.min(max, value)) : undefined;

export function loadControllerLayout(): ControllerLayout {
  try {
    const stored = JSON.parse(localStorage.getItem(CONTROLLER_LAYOUT_KEY) ?? "null") as Partial<ControllerLayout> | null;
    if (!stored) return defaultLayout();
    const layout = defaultLayout(stored.preset === "mirrored" ? "mirrored" : "default");
    layout.opacity = bounded(stored.opacity, 20, 100);
    layout.swapSticks = stored.swapSticks === true;
    for (const id of CONTROLS) {
      const placement = stored.placements?.[id];
      if (placement) {
        layout.placements[id] = {
          x: bounded(placement.x, 0, 100), y: bounded(placement.y, 0, 100),
          scale: bounded(placement.scale, 60, 150), hidden: placement.hidden === true,
        };
      }
    }
    for (const id of BUTTONS) {
      const binding = stored.bindings?.[id];
      if (binding && BUTTONS.includes(binding)) layout.bindings[id] = binding;
    }
    return layout;
  } catch {
    return defaultLayout();
  }
}
