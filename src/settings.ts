import type { UiSettings } from "./types";

export const LIMITS = {
  intervalMin: 10,
  intervalMax: 3_600_000,
  clicksMax: 999_999_999,
  loopsMax: 1_000_000,
  coordMin: -100_000,
  coordMax: 100_000,
} as const;

export const JITTER_STEPS = ["0", "10", "25", "50"];
export const SPEED_STEPS = ["0.5x", "1x", "2x", "4x"];

export const DEFAULT_HOTKEYS = {
  clicker: "f6",
  record: "f8",
  play: "f9",
  hold: "f7",
} as const;

export const DEFAULT_UI: UiSettings = {
  interval: "100",
  button: "左键",
  double: "单击",
  pos_mode: 0,
  pos_x: "",
  pos_y: "",
  cnt_mode: 1,
  count: "100",
  jitter: "0",
  speed: "1x",
  loops: "1",
  moves: false,
  top: false,
  sound: true,
};

export function toInt(value: unknown): number | null {
  const text = String(value ?? "").trim();
  if (!/^-?\d+$/.test(text)) return null;
  const n = Number(text);
  return Number.isSafeInteger(n) ? n : null;
}

export function clamp(n: number, lo: number, hi: number) {
  return Math.min(Math.max(n, lo), hi);
}

export function sanitizeUi(raw: unknown): UiSettings {
  const cfg: UiSettings = { ...DEFAULT_UI };
  if (!raw || typeof raw !== "object") return cfg;
  const obj = raw as Record<string, unknown>;

  const interval = toInt(obj.interval);
  if (interval !== null) {
    cfg.interval = String(clamp(interval, LIMITS.intervalMin, LIMITS.intervalMax));
  }
  if (obj.button === "左键" || obj.button === "中键" || obj.button === "右键") {
    cfg.button = obj.button;
  }
  if (obj.double === "单击" || obj.double === "双击") cfg.double = obj.double;
  if (obj.pos_mode === 0 || obj.pos_mode === 1) cfg.pos_mode = obj.pos_mode;

  for (const key of ["pos_x", "pos_y"] as const) {
    const text = String(obj[key] ?? "").trim();
    if (!text) {
      cfg[key] = "";
      continue;
    }
    const n = toInt(text);
    cfg[key] =
      n === null ? "" : String(clamp(n, LIMITS.coordMin, LIMITS.coordMax));
  }

  if (obj.cnt_mode === 0 || obj.cnt_mode === 1) cfg.cnt_mode = obj.cnt_mode;
  const count = toInt(obj.count);
  if (count !== null) {
    const minCount = cfg.cnt_mode === 1 ? 1 : 0;
    cfg.count = String(clamp(count, minCount, LIMITS.clicksMax));
  }
  if (JITTER_STEPS.includes(String(obj.jitter))) cfg.jitter = String(obj.jitter);
  if (SPEED_STEPS.includes(String(obj.speed))) cfg.speed = String(obj.speed);
  const loops = toInt(obj.loops);
  if (loops !== null) cfg.loops = String(clamp(loops, 0, LIMITS.loopsMax));
  if (typeof obj.moves === "boolean") cfg.moves = obj.moves;
  if (typeof obj.top === "boolean") cfg.top = obj.top;
  if (typeof obj.sound === "boolean") cfg.sound = obj.sound;
  return cfg;
}
