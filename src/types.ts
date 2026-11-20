export type ButtonCn = "左键" | "中键" | "右键";
export type Action = "clicker" | "record" | "play" | "hold";
export type TaskKind = Action;
export type SoundKind = "start" | "stop" | "tick" | "done";
export type InputHealth = "unknown" | "requested" | "installing" | "ready" | "failed" | "stopped";

export interface UiSettings {
  interval: string;
  button: ButtonCn;
  double: "单击" | "双击";
  pos_mode: 0 | 1;
  pos_x: string;
  pos_y: string;
  cnt_mode: 0 | 1;
  count: string;
  jitter: string;
  speed: string;
  loops: string;
  moves: boolean;
  top: boolean;
  sound: boolean;
}

export interface ScriptEvent {
  type: "click" | "move" | "scroll" | "wait";
  dt?: number;
  x: number;
  y: number;
  button?: "left" | "middle" | "right";
  pressed?: boolean;
  dx?: number;
  dy?: number;
}

export interface ScriptInfo {
  fileName: string;
  name: string;
  count: number;
  saved: string;
  available: boolean;
  reason: string | null;
}

export interface ScriptLibraryPage {
  items: ScriptInfo[];
  total: number;
  offset: number;
  limit: number;
  snapshot: string;
}

export interface ScriptPage {
  fileName: string;
  total: number;
  offset: number;
  events: ScriptEvent[];
}

export interface RecordResult {
  events: ScriptEvent[];
  savedName: string | null;
  saveError?: string | null;
  overflowed?: boolean;
  pendingSave?: boolean;
}

export interface PendingRecordingInfo {
  eventCount: number;
  overflowed: boolean;
  error: string;
}

export interface SavedRecording {
  savedName: string;
  eventCount: number;
}

export interface ClosePreparation {
  ready: boolean;
  pendingRecording: PendingRecordingInfo | null;
  error: string | null;
}

export interface LoadResult {
  doc: SettingsDoc | null;
  warning: string | null;
}

export interface InputStatus {
  keyboard: boolean;
  mouse: boolean;
  keyboardState: InputHealth;
  mouseState: InputHealth;
  error: string | null;
}

export interface TaskSnapshot {
  running: boolean;
  kind: TaskKind | null;
  id: number | null;
  stopping: boolean;
}

export interface TrashEntry {
  id: string;
  fileName: string;
  name: string;
  deleted: string;
}

export type HotkeyMap = Record<Action, string>;

export interface SettingsDoc {
  hotkeys?: Partial<Record<Action, string>>;
  hold_button?: string;
  ui?: unknown;
}
