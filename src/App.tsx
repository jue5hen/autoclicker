import {
  memo,
  useCallback,
  useEffect,
  useRef,
  useState,
  type KeyboardEvent as ReactKeyboardEvent,
  type ReactNode,
} from "react";
import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { getCurrentWindow } from "@tauri-apps/api/window";
import {
  comboText,
  hotkeyFromEvent,
  isModifierKey,
  normalizeCombo,
  validateHotkey,
} from "./hotkey";
import {
  DEFAULT_HOTKEYS,
  DEFAULT_UI,
  JITTER_STEPS,
  LIMITS,
  SPEED_STEPS,
  clamp,
  sanitizeUi,
  toInt,
} from "./settings";
import type {
  Action,
  ButtonCn,
  HotkeyMap,
  InputStatus,
  LoadResult,
  ClosePreparation,
  PendingRecordingInfo,
  RecordResult,
  SavedRecording,
  ScriptEvent,
  ScriptInfo,
  ScriptLibraryPage,
  ScriptPage,
  SoundKind,
  TaskKind,
  TaskSnapshot,
  TrashEntry,
  UiSettings,
} from "./types";

const APP_NAME = "AutoClicker";
const VERSION = "v1.0.0";
const PANIC_HINT = "急停：鼠标甩到主屏左上角 3×3 像素";
const MAX_ROWS = 2000;
const LIBRARY_PAGE_SIZE = 100;

type DialogState =
  | { kind: "rename" | "delete"; file: string; name: string }
  | { kind: "unsaved-recording"; closeAfter: boolean }
  | { kind: "settings-save"; error: string }
  | { kind: "close-error"; message: string }
  | { kind: "trash"; entries: TrashEntry[] };

const ACTIONS: Action[] = ["clicker", "record", "play", "hold"];
const BTN_CN = { left: "左键", middle: "中键", right: "右键" } as const;
const BTN_VALUE: Record<ButtonCn, "left" | "middle" | "right"> = {
  左键: "left",
  中键: "middle",
  右键: "right",
};
const ACTION_CN: Record<Action, string> = {
  clicker: "连点",
  record: "录制",
  play: "回放",
  hold: "长按",
};
const KIND_CN: Record<TaskKind, string> = ACTION_CN;

function blockHotkeyActivation(event: ReactKeyboardEvent<HTMLButtonElement>) {
  if (event.key === " " || event.key === "Enter") event.preventDefault();
}

function arrowText(ev: ScriptEvent) {
  if (ev.type === "click") {
    return `${BTN_CN[ev.button ?? "left"]}${ev.pressed ? "按下" : "抬起"}`;
  }
  if (ev.type === "move") return "移动";
  if (ev.type === "wait") return "等待";
  return `滚轮 (x${(ev.dx ?? 0) >= 0 ? "+" : ""}${ev.dx ?? 0}, y${(ev.dy ?? 0) >= 0 ? "+" : ""}${ev.dy ?? 0})`;
}

function rowPosition(ev: ScriptEvent) {
  if (ev.type === "wait") return "等待段";
  if (ev.type === "scroll") {
    const dx = ev.dx ?? 0;
    const dy = ev.dy ?? 0;
    return `(${ev.x}, ${ev.y}) Δ(${dx >= 0 ? "+" : ""}${dx},${dy >= 0 ? "+" : ""}${dy})`;
  }
  return `(${ev.x}, ${ev.y})`;
}

function sleep(ms: number) {
  return new Promise((resolve) => window.setTimeout(resolve, ms));
}

function Card({
  title,
  accent,
  headRight,
  className,
  children,
}: {
  title: string;
  accent: string;
  headRight?: ReactNode;
  className?: string;
  children: ReactNode;
}) {
  return (
    <div className={`card${className ? ` ${className}` : ""}`}>
      <div className="card-head">
        <span className="accent" style={{ background: accent }} />
        <span className="card-title">{title}</span>
        {headRight ? <div className="head-right">{headRight}</div> : null}
      </div>
      {children}
    </div>
  );
}

function Segmented({
  options,
  value,
  onChange,
}: {
  options: string[];
  value: string;
  onChange: (value: string) => void;
}) {
  return (
    <div className="segmented">
      {options.map((option) => (
        <button
          type="button"
          key={option}
          aria-pressed={value === option}
          className={value === option ? "active" : ""}
          onClick={() => onChange(option)}
        >
          {option}
        </button>
      ))}
    </div>
  );
}

function Switch({
  checked,
  label,
  onChange,
}: {
  checked: boolean;
  label: string;
  onChange: (value: boolean) => void;
}) {
  return (
    <div className="switch-wrap" onClick={() => onChange(!checked)}>
      <button
        type="button"
        role="switch"
        aria-checked={checked}
        aria-label={label}
        className={`switch${checked ? " on" : ""}`}
        onClick={(event) => {
          event.stopPropagation();
          onChange(!checked);
        }}
      />
      <span>{label}</span>
    </div>
  );
}

const EventRow = memo(function EventRow({ ev, index }: { ev: ScriptEvent; index: number }) {
  return (
    <tr>
      <td>{index + 1}</td>
      <td>{ev.dt ?? 0}</td>
      <td>{arrowText(ev)}</td>
      <td className="left">{rowPosition(ev)}</td>
    </tr>
  );
});

export default function App() {
  const [ui, setUiState] = useState<UiSettings>(DEFAULT_UI);
  const uiRef = useRef<UiSettings>(DEFAULT_UI);
  const [hotkeys, setHotkeysState] = useState<HotkeyMap>(DEFAULT_HOTKEYS);
  const hotkeysRef = useRef<HotkeyMap>(DEFAULT_HOTKEYS);
  const [holdButton, setHoldButtonState] = useState<ButtonCn>("左键");
  const holdButtonRef = useRef<ButtonCn>("左键");
  const [events, setEventsState] = useState<ScriptEvent[]>([]);
  const eventsRef = useRef<ScriptEvent[]>([]);
  const [eventTotal, setEventTotal] = useState(0);
  const [activeLibraryFile, setActiveLibraryFileState] = useState<string | null>(null);
  const activeLibraryFileRef = useRef<string | null>(null);
  const loadSeqRef = useRef(0);
  const [progress, setProgress] = useState<string | null>(null);
  const [running, setRunningState] = useState<TaskKind | null>(null);
  const runningRef = useRef<TaskKind | null>(null);
  const runningTaskIdRef = useRef<number | null>(null);
  const [clickCount, setClickCount] = useState(0);
  const [capture, setCaptureState] = useState<Action | null>(null);
  const captureRef = useRef<Action | null>(null);
  const [counting, setCounting] = useState(false);
  const countingRef = useRef(false);
  const [status, setStatusState] = useState({ text: "就绪", warn: false });
  const [cursor, setCursor] = useState<[number, number] | null>(null);
  const [library, setLibrary] = useState<ScriptInfo[]>([]);
  const [libraryTotal, setLibraryTotal] = useState(0);
  const [libraryOffset, setLibraryOffset] = useState(0);
  const libraryOffsetRef = useRef(0);
  const librarySnapshotRef = useRef<string | null>(null);
  const [selected, setSelected] = useState<string | null>(null);
  const [pendingSave, setPendingSave] = useState<PendingRecordingInfo | null>(null);
  const [admin, setAdmin] = useState<boolean | null>(null);
  const [inputStatus, setInputStatus] = useState<InputStatus | null>(null);
  const [loaded, setLoaded] = useState(false);
  const [dialog, setDialog] = useState<DialogState | null>(null);
  const [renameValue, setRenameValue] = useState("");
  const handlersRef = useRef<Record<Action, () => void>>({
    clicker: () => {},
    record: () => {},
    play: () => {},
    hold: () => {},
  });
  const capturedRef = useRef<(combo: string | null) => void>(() => {});
  const saveFailedRef = useRef(false);
  const saveInFlightRef = useRef<Promise<void> | null>(null);
  const saveQueuedRef = useRef(false);
  const closeInFlightRef = useRef(false);
  const closeRequestedRef = useRef(false);
  const modalRef = useRef<HTMLDivElement | null>(null);

  const setStatus = useCallback((text: string, warn = false) => {
    setStatusState({ text, warn });
  }, []);

  const applyUi = useCallback((patch: Partial<UiSettings>) => {
    const next = { ...uiRef.current, ...patch };
    uiRef.current = next;
    setUiState(next);
  }, []);

  const applyHotkeys = useCallback((next: HotkeyMap) => {
    hotkeysRef.current = next;
    setHotkeysState(next);
  }, []);

  const setRunning = useCallback((kind: TaskKind | null) => {
    runningRef.current = kind;
    if (kind === null) runningTaskIdRef.current = null;
    setRunningState(kind);
  }, []);

  const setActiveLibraryFile = useCallback((file: string | null) => {
    activeLibraryFileRef.current = file;
    setActiveLibraryFileState(file);
  }, []);

  const setEvents = useCallback(
    (next: ScriptEvent[], total = next.length, libraryFile: string | null = null) => {
      eventsRef.current = next;
      setActiveLibraryFile(libraryFile);
      setEventsState(next);
      setEventTotal(total);
    },
    [setActiveLibraryFile],
  );

  const beep = useCallback((kind: SoundKind) => {
    if (!uiRef.current.sound) return;
    void invoke("beep", { kind, sound: true }).catch(() => {});
  }, []);

  const buildSettings = useCallback(() => {
    const c = uiRef.current;
    return {
      hotkeys: hotkeysRef.current,
      hold_button: BTN_VALUE[holdButtonRef.current],
      ui: {
        interval: c.interval,
        button: c.button,
        double: c.double,
        pos_mode: c.pos_mode,
        pos_x: c.pos_x,
        pos_y: c.pos_y,
        cnt_mode: c.cnt_mode,
        count: c.count,
        jitter: c.jitter,
        speed: c.speed,
        loops: c.loops,
        moves: c.moves,
        top: c.top,
        sound: c.sound,
      },
    };
  }, []);

  const saveNow = useCallback(async () => {
    if (saveInFlightRef.current) {
      saveQueuedRef.current = true;
      return saveInFlightRef.current;
    }
    const task = (async () => {
      let lastError: unknown = null;
      do {
        saveQueuedRef.current = false;
        try {
          // 每次重跑都重新读取最新 refs，确保 flush 的是最后确认值。
          await invoke("save_settings", { settings: buildSettings() });
          saveFailedRef.current = false;
          lastError = null;
        } catch (err) {
          lastError = err;
          // 只提示一次，避免每次 debounce 都把状态栏刷成红色。
          if (!saveFailedRef.current) {
            saveFailedRef.current = true;
            setStatus(`设置保存失败：${String(err)}`, true);
          }
        }
      } while (saveQueuedRef.current);
      if (lastError) throw lastError;
    })();
    saveInFlightRef.current = task;
    try {
      await task;
    } finally {
      if (saveInFlightRef.current === task) saveInFlightRef.current = null;
    }
  }, [buildSettings, setStatus]);

  const refreshLibrary = useCallback(
    async (
      prefer?: string,
      requestedOffset = libraryOffsetRef.current,
      forceNewSnapshot = false,
    ) => {
      const fetchPage = (offset: number, snapshot: string | null) =>
        invoke<ScriptLibraryPage>("list_scripts_page", {
          offset,
          limit: LIBRARY_PAGE_SIZE,
          snapshot,
        });
      try {
        let snapshot = forceNewSnapshot ? null : librarySnapshotRef.current;
        let page = await fetchPage(requestedOffset, snapshot);
        if (page.items.length === 0 && page.total > 0 && page.offset > 0) {
          page = await fetchPage(Math.max(0, page.offset - LIBRARY_PAGE_SIZE), page.snapshot);
        }
        librarySnapshotRef.current = page.snapshot;
        libraryOffsetRef.current = page.offset;
        setLibraryOffset(page.offset);
        setLibraryTotal(page.total);
        setLibrary(page.items);
        setSelected((prev) => {
          const target = prefer ?? prev;
          return target && page.items.some((row) => row.fileName === target) ? target : null;
        });
      } catch (err) {
        setStatus(`脚本库刷新失败：${String(err)}`, true);
      }
    },
    [setStatus],
  );
  const toggleClicker = useCallback(async () => {
    if (runningRef.current === "clicker") {
      void invoke("stop_clicker").catch(() => {});
      setProgress("正在停止…");
      beep("stop");
      return;
    }
    const busy = runningRef.current;
    if (busy) {
      setStatus(`无法开始连点：${KIND_CN[busy]}正在进行`, true);
      return;
    }
    const cfg = uiRef.current;
    const ms = toInt(cfg.interval);
    if (ms === null || ms <= 0) {
      setStatus("请输入有效的点击间隔（毫秒）", true);
      return;
    }
    let interval = ms;
    let note = "";
    if (interval < LIMITS.intervalMin || interval > LIMITS.intervalMax) {
      interval = clamp(interval, LIMITS.intervalMin, LIMITS.intervalMax);
      note = `间隔已按 ${LIMITS.intervalMin}–${LIMITS.intervalMax} 毫秒范围执行`;
    }
    let position: [number, number] | null = null;
    if (cfg.pos_mode === 1) {
      const x = toInt(cfg.pos_x);
      const y = toInt(cfg.pos_y);
      if (
        x === null ||
        y === null ||
        x < LIMITS.coordMin ||
        x > LIMITS.coordMax ||
        y < LIMITS.coordMin ||
        y > LIMITS.coordMax
      ) {
        setStatus("请填写 -100000–100000 范围内的有效坐标", true);
        return;
      }
      position = [x, y];
    }
    let limit = 0;
    if (cfg.cnt_mode === 1) {
      limit = toInt(cfg.count) ?? 0;
      if (limit <= 0 || limit > LIMITS.clicksMax) {
        setStatus(`点击次数需在 1–${LIMITS.clicksMax} 之间（或选择无限）`, true);
        return;
      }
    }
    const jitter = (toInt(cfg.jitter) ?? 0) / 100;
    try {
      const id = await invoke<number>("start_clicker", {
        interval,
        button: BTN_VALUE[cfg.button],
        double: cfg.double === "双击",
        position,
        limit,
        jitter,
      });
      runningTaskIdRef.current = id;
      setRunning("clicker");
      setClickCount(0);
      setProgress("运行中");
      beep("start");
      setStatus(`连点中…（${PANIC_HINT}）` + (note ? `${note}。` : ""));
    } catch (err) {
      setStatus(String(err), true);
    }
  }, [beep, setRunning, setStatus]);

  const toggleRecord = useCallback(
    async (viaButton: boolean) => {
      if (runningRef.current === "record") {
        beep("stop");
        try {
          const result = await invoke<RecordResult>("stop_recording", { trim: viaButton });
          setRunning(null);
          setProgress(null);
          setEvents(result.events);
          if (result.pendingSave && result.saveError) {
            setPendingSave({
              eventCount: result.events.length,
              overflowed: Boolean(result.overflowed),
              error: result.saveError,
            });
            setDialog({ kind: "unsaved-recording", closeAfter: false });
          } else {
            setPendingSave(null);
          }
          if (!result.events.length) {
            setStatus(result.overflowed ? "录制达到上限，但没有可保存事件" : "未录到事件", true);
            return;
          }
          void refreshLibrary(undefined, 0, true);
          const limitNote = result.overflowed
            ? `；录制达到 ${result.events.length} 条上限，后续事件未记录`
            : "";
          if (result.saveError) {
            setStatus(
              `录制完成，共 ${result.events.length} 个事件，但保存失败：${result.saveError}${limitNote}`,
              true,
            );
          } else {
            const saved = result.savedName ? `，已自动保存 ${result.savedName}` : "";
            setStatus(`录制完成，共 ${result.events.length} 个事件${saved}${limitNote}`, result.overflowed);
          }
        } catch (err) {
          setStatus(String(err), true);
        }
        return;
      }
      const busy = runningRef.current;
      if (busy) {
        setStatus(`无法开始录制：${KIND_CN[busy]}正在进行`, true);
        return;
      }
      if (pendingSave) {
        setDialog({ kind: "unsaved-recording", closeAfter: false });
        return;
      }
      try {
        const id = await invoke<number>("start_recording", { moves: uiRef.current.moves });
        runningTaskIdRef.current = id;
        setEvents([]);
        setProgress(null);
        setRunning("record");
        setProgress("已记录 0 个事件");
        beep("start");
        setStatus("录制中…（热键停止；请勿点击本窗口）");
      } catch (err) {
        setStatus(String(err), true);
      }
    },
    [beep, pendingSave, refreshLibrary, setEvents, setRunning, setStatus],
  );

  const togglePlay = useCallback(async () => {
    if (runningRef.current === "play") {
      void invoke("stop_playback").catch(() => {});
      setProgress("正在停止…");
      beep("stop");
      return;
    }
    const busy = runningRef.current;
    if (busy) {
      setStatus(`无法开始回放：${KIND_CN[busy]}正在进行`, true);
      return;
    }
    if (!eventsRef.current.length) {
      setStatus("没有可回放的事件，请先录制或在「脚本库」加载", true);
      return;
    }
    const cfg = uiRef.current;
    const speed = Number.parseFloat(cfg.speed) || 1;
    const loops = toInt(cfg.loops);
    if (loops === null || loops < 0 || loops > LIMITS.loopsMax) {
      setStatus(`循环次数需在 0–${LIMITS.loopsMax} 之间（0=无限）`, true);
      return;
    }
    try {
      const libraryFile = activeLibraryFileRef.current;
      const id = libraryFile
        ? await invoke<number>("start_playback_file", { file: libraryFile, speed, loops })
        : await invoke<number>("start_playback", {
            events: eventsRef.current,
            speed,
            loops,
          });
      runningTaskIdRef.current = id;
      setRunning("play");
      setProgress(`第 1${loops ? `/${loops}` : ""} 遍`);
      beep("start");
      setStatus(`回放中… 第 1${loops ? `/${loops}` : ""} 遍`);
    } catch (err) {
      setStatus(String(err), true);
    }
  }, [beep, setRunning, setStatus]);

  const toggleHold = useCallback(async () => {
    if (runningRef.current === "hold") {
      try {
        const stopped = await invoke<boolean>("stop_hold");
        if (!stopped) {
          setRunning(null);
          setProgress(null);
          setStatus("长按已松开");
        } else {
          setProgress("正在松开…");
        }
      } catch (err) {
        setStatus(String(err), true);
      }
      return;
    }
    const busy = runningRef.current;
    if (busy) {
      setStatus(`无法开始长按：${KIND_CN[busy]}正在进行`, true);
      return;
    }
    try {
      const id = await invoke<number>("start_hold", {
        button: BTN_VALUE[holdButtonRef.current],
      });
      runningTaskIdRef.current = id;
      setRunning("hold");
      setProgress("按住中");
      beep("start");
      setStatus(`长按中…（再按热键松开；${PANIC_HINT}）`);
    } catch (err) {
      setStatus(String(err), true);
    }
  }, [beep, setRunning, setStatus]);

  const changeHoldButton = useCallback(
    async (value: ButtonCn) => {
      if (value === holdButtonRef.current) return;
      if (runningRef.current === "hold") {
        try {
          const stopped = await invoke<boolean>("stop_hold");
          if (!stopped) {
            setRunning(null);
            setProgress(null);
          }
        } catch (err) {
          setStatus(`无法切换长按按键：${String(err)}`, true);
          return;
        }
      }
      holdButtonRef.current = value;
      setHoldButtonState(value);
    },
    [setRunning, setStatus],
  );

  const startCapture = useCallback(
    async (action: Action) => {
      captureRef.current = action;
      setCaptureState(action);
      setStatus(`设置「${ACTION_CN[action]}」热键：请按下新的组合键（Esc 取消）`);
      try {
        await invoke("start_hotkey_capture");
      } catch (err) {
        captureRef.current = null;
        setCaptureState(null);
        setStatus(String(err), true);
      }
    },
    [setStatus],
  );

  const handleCaptureDone = useCallback(
    async (raw: string | null) => {
      const action = captureRef.current;
      if (!action) return;
      captureRef.current = null;
      setCaptureState(null);
      if (!raw) {
        setStatus("已取消热键设置");
        return;
      }
      const normalized = normalizeCombo(raw);
      const validation = validateHotkey(normalized);
      if (!validation.ok) {
        setStatus(`设置失败：${validation.reason}`, true);
        return;
      }
      const combo = validation.combo;
      const clash = (Object.keys(hotkeysRef.current) as Action[]).find(
        (key) => key !== action && hotkeysRef.current[key] === combo,
      );
      if (clash) {
        setStatus(`设置失败：${comboText(combo)} 已被「${ACTION_CN[clash]}」使用`, true);
        return;
      }
      if (runningRef.current === "hold") {
        try {
          await invoke("stop_hold");
        } catch {
          /* 保持原状 */
        }
      }
      const next = { ...hotkeysRef.current, [action]: combo };
      try {
        await invoke("set_hotkeys", { hotkeys: next });
        applyHotkeys(next);
      } catch (err) {
        setStatus(String(err), true);
        return;
      }
      setStatus(`已将「${ACTION_CN[action]}」热键设为 ${comboText(combo)}`);
    },
    [applyHotkeys, setStatus],
  );
  capturedRef.current = (combo) => {
    void handleCaptureDone(combo);
  };

  useEffect(() => {
    if (!capture) return;
    const action = capture;
    let fallback = 0;
    const onKeyDown = (event: KeyboardEvent) => {
      if (event.key === "Escape") {
        event.preventDefault();
        event.stopPropagation();
        window.clearTimeout(fallback);
        fallback = window.setTimeout(() => {
          if (captureRef.current !== action) return;
          void invoke("cancel_hotkey_capture", { mute: true }).catch(() => {});
          void handleCaptureDone(null);
        }, 350);
        return;
      }
      if (isModifierKey(event)) return;
      const combo = hotkeyFromEvent(event);
      event.preventDefault();
      event.stopPropagation();
      if (!combo) {
        setStatus("这个按键不支持，请换 F1–F12、字母、数字或常用符号", true);
        return;
      }
      window.clearTimeout(fallback);
      fallback = window.setTimeout(() => {
        if (captureRef.current !== action) return;
        void invoke("cancel_hotkey_capture", { mute: true }).catch(() => {});
        void handleCaptureDone(combo);
      }, 350);
    };
    const onKeyUp = (event: KeyboardEvent) => {
      if (event.key === "Escape" || !isModifierKey(event)) {
        event.preventDefault();
        event.stopPropagation();
      }
    };
    window.addEventListener("keydown", onKeyDown, true);
    window.addEventListener("keyup", onKeyUp, true);
    return () => {
      window.clearTimeout(fallback);
      window.removeEventListener("keydown", onKeyDown, true);
      window.removeEventListener("keyup", onKeyUp, true);
    };
  }, [capture, handleCaptureDone, setStatus]);

  handlersRef.current = {
    clicker: () => void toggleClicker(),
    record: () => void toggleRecord(false),
    play: () => void togglePlay(),
    hold: () => void toggleHold(),
  };

  const pickPosition = useCallback(async () => {
    if (countingRef.current) return;
    countingRef.current = true;
    setCounting(true);
    try {
      for (let n = 3; n > 0; n -= 1) {
        setStatus(`${n} 秒后捕获光标位置，请将鼠标移到目标上…`);
        beep("tick");
        await sleep(1000);
      }
      const pos = await invoke<[number, number]>("get_cursor");
      applyUi({ pos_mode: 1, pos_x: String(pos[0]), pos_y: String(pos[1]) });
      beep("done");
      setStatus(`已捕获坐标 (${pos[0]}, ${pos[1]})`);
    } catch (err) {
      setStatus(String(err), true);
    } finally {
      countingRef.current = false;
      setCounting(false);
    }
  }, [applyUi, beep, setStatus]);

  const clearEvents = useCallback(() => {
    if (pendingSave) {
      setDialog({ kind: "unsaved-recording", closeAfter: false });
      setStatus("请先保存或放弃当前录制", true);
      return;
    }
    setEvents([]);
    setStatus("事件列表已清空");
  }, [pendingSave, setEvents, setStatus]);

  const loadScript = useCallback(
    async (file: string) => {
      const row = library.find((item) => item.fileName === file);
      if (!row?.available) {
        setStatus(row?.reason ?? "脚本不可用，请刷新脚本库", true);
        return;
      }
      const busy = runningRef.current;
      if (busy) {
        setStatus(`无法加载脚本：${KIND_CN[busy]}正在进行`, true);
        return;
      }
      if (pendingSave) {
        setDialog({ kind: "unsaved-recording", closeAfter: false });
        setStatus("请先保存或放弃当前录制", true);
        return;
      }
      const seq = ++loadSeqRef.current;
      setSelected(file);
      try {
        const page = await invoke<ScriptPage>("load_script_page", {
          file,
          offset: 0,
          limit: MAX_ROWS,
        });
        if (seq !== loadSeqRef.current) return;
        // 只有加载成功才切换“活动脚本”；单击选择不等于已加载。
        setEvents(page.events, page.total, file);
        setStatus(
          `已加载脚本：${file.slice(0, -5)}（${page.total} 个事件），可按热键回放`,
        );
      } catch (err) {
        if (seq === loadSeqRef.current) setStatus(String(err), true);
      }
    },
    [library, pendingSave, setEvents, setStatus],
  );

  const loadSelected = useCallback(() => {
    if (!selected) {
      setStatus("请先在列表中选择一个脚本", true);
      return;
    }
    void loadScript(selected);
  }, [loadScript, selected, setStatus]);

  const selectedRow = library.find((row) => row.fileName === selected);

  const openRename = useCallback(() => {
    if (!selectedRow) {
      setStatus("请先在列表中选择一个脚本", true);
      return;
    }
    setRenameValue(selectedRow.name);
    setDialog({ kind: "rename", file: selectedRow.fileName, name: selectedRow.name });
  }, [selectedRow, setStatus]);

  const openDelete = useCallback(() => {
    if (!selectedRow) {
      setStatus("请先在列表中选择一个脚本", true);
      return;
    }
    setDialog({ kind: "delete", file: selectedRow.fileName, name: selectedRow.name });
  }, [selectedRow, setStatus]);

  const openTrash = useCallback(async () => {
    try {
      const entries = await invoke<TrashEntry[]>("list_trash");
      setDialog({ kind: "trash", entries });
    } catch (err) {
      setStatus(String(err), true);
    }
  }, [setStatus]);

  const restoreTrash = useCallback(
    async (entry: TrashEntry) => {
      try {
        const restored = await invoke<string>("restore_script", { id: entry.id });
        setDialog(null);
        await refreshLibrary(restored, 0, true);
        setSelected(restored);
        setStatus(`已从回收站恢复：${restored}`);
      } catch (err) {
        setStatus(String(err), true);
      }
    },
    [refreshLibrary, setStatus],
  );

  const confirmDialog = useCallback(async () => {
    if (!dialog) return;
    if (dialog.kind === "delete") {
      const target = dialog;
      setDialog(null);
      try {
        await invoke("delete_script", { file: target.file });
        await refreshLibrary(undefined, libraryOffsetRef.current, true);
        setSelected(null);
        if (activeLibraryFileRef.current === target.file) setEvents([]);
        setStatus(`已移到回收站：${target.name}`);
      } catch (err) {
        setStatus(String(err), true);
      }
      return;
    }
    if (dialog.kind !== "rename") return;
    const target = dialog;
    const newName = renameValue.trim();
    if (!newName || newName === target.name) {
      setDialog(null);
      return;
    }
    setDialog(null);
    try {
      const newFile = await invoke<string>("rename_script", {
        file: target.file,
        newName,
      });
      await refreshLibrary(newFile, 0, true);
      setSelected(newFile);
      if (activeLibraryFileRef.current === target.file) {
        setActiveLibraryFile(newFile);
      }
      setStatus(`已重命名：${target.name} → ${newName}`);
    } catch (err) {
      setStatus(String(err), true);
    }
  }, [dialog, refreshLibrary, renameValue, setActiveLibraryFile, setEvents, setStatus]);

  const openFolder = useCallback(async () => {
    try {
      await invoke("open_scripts_folder");
    } catch (err) {
      setStatus(String(err), true);
    }
  }, [setStatus]);

  const cancelCloseFlow = useCallback(() => {
    closeRequestedRef.current = false;
    closeInFlightRef.current = false;
    void invoke("cancel_close").catch(() => {});
  }, []);

  const performClose = useCallback(async () => {
    if (closeInFlightRef.current) return;
    closeRequestedRef.current = true;
    closeInFlightRef.current = true;
    setStatus("正在准备退出…");
    let prep: ClosePreparation;
    try {
      prep = await invoke<ClosePreparation>("prepare_close");
      setRunning(null);
      setProgress(null);
      setPendingSave(prep.pendingRecording);
    } catch (err) {
      setDialog({ kind: "close-error", message: String(err) });
      closeInFlightRef.current = false;
      return;
    }
    if (prep.pendingRecording) {
      setPendingSave(prep.pendingRecording);
      setDialog({ kind: "unsaved-recording", closeAfter: true });
      closeInFlightRef.current = false;
      return;
    }
    if (prep.error) {
      setDialog({ kind: "close-error", message: prep.error });
      closeInFlightRef.current = false;
      return;
    }
    try {
      // 退出前必须 flush 最新设置修订；失败时停留窗口并给出明确选择。
      await saveNow();
    } catch (err) {
      setDialog({ kind: "settings-save", error: String(err) });
      closeInFlightRef.current = false;
      return;
    }
    try {
      await invoke("finish_close");
    } catch (err) {
      setDialog({ kind: "close-error", message: String(err) });
      closeInFlightRef.current = false;
    }
  }, [saveNow, setRunning, setStatus]);

  const retryPendingSave = useCallback(async () => {
    try {
      const saved = await invoke<SavedRecording>("retry_pending_recording");
      setPendingSave(null);
      setDialog(null);
      await refreshLibrary(saved.savedName, 0, true);
      setStatus(`已保存录制：${saved.savedName}（${saved.eventCount} 个事件）`);
      if (closeRequestedRef.current) {
        closeInFlightRef.current = false;
        void performClose();
      }
    } catch (err) {
      setPendingSave((prev) =>
        prev ? { ...prev, error: String(err) } : { eventCount: 0, overflowed: false, error: String(err) },
      );
      setStatus(`重试保存失败：${String(err)}`, true);
    }
  }, [performClose, refreshLibrary, setStatus]);

  const savePendingAs = useCallback(async () => {
    try {
      const saved = await invoke<SavedRecording | null>("save_pending_recording_as");
      if (!saved) {
        setStatus("已取消另存");
        return;
      }
      setPendingSave(null);
      setDialog(null);
      await refreshLibrary(saved.savedName, 0);
      setStatus(`已另存录制：${saved.savedName}（${saved.eventCount} 个事件）`);
      if (closeRequestedRef.current) {
        closeInFlightRef.current = false;
        void performClose();
      }
    } catch (err) {
      setStatus(`另存失败：${String(err)}`, true);
    }
  }, [performClose, refreshLibrary, setStatus]);

  const discardPending = useCallback(async () => {
    try {
      await invoke("discard_pending_recording_command");
      setPendingSave(null);
      setDialog(null);
      setStatus("已放弃未保存的录制");
      if (closeRequestedRef.current) {
        closeInFlightRef.current = false;
        void performClose();
      }
    } catch (err) {
      setStatus(String(err), true);
    }
  }, [performClose, setStatus]);

  const cancelDialog = useCallback(() => {
    const cancelsClose =
      dialog?.kind === "settings-save" ||
      dialog?.kind === "close-error" ||
      (dialog?.kind === "unsaved-recording" && dialog.closeAfter);
    setDialog(null);
    if (cancelsClose) {
      cancelCloseFlow();
      setStatus("已取消退出");
    }
  }, [cancelCloseFlow, dialog, setStatus]);

  const retryClose = useCallback(() => {
    setDialog(null);
    closeInFlightRef.current = false;
    void performClose();
  }, [performClose]);

  const exitWithoutSettings = useCallback(() => {
    setDialog(null);
    void invoke("finish_close").catch((err) => {
      setDialog({ kind: "close-error", message: String(err) });
    });
  }, []);

  const elevate = useCallback(async () => {
    setStatus("正在准备管理员权限交接…");
    let prep: ClosePreparation;
    try {
      prep = await invoke<ClosePreparation>("prepare_close");
      setRunning(null);
      setProgress(null);
      setPendingSave(prep.pendingRecording);
    } catch (err) {
      setStatus(String(err), true);
      return;
    }
    if (prep.pendingRecording) {
      await invoke("cancel_close").catch(() => {});
      setPendingSave(prep.pendingRecording);
      setDialog({ kind: "unsaved-recording", closeAfter: false });
      setStatus("提权已取消：请先保存或放弃当前录制", true);
      return;
    }
    if (prep.error) {
      await invoke("cancel_close").catch(() => {});
      setDialog({ kind: "close-error", message: prep.error });
      return;
    }
    try {
      await saveNow();
    } catch (err) {
      await invoke("cancel_close").catch(() => {});
      setDialog({ kind: "settings-save", error: String(err) });
      return;
    }
    try {
      await invoke("elevate");
    } catch (err) {
      await invoke("cancel_close").catch(() => {});
      setStatus(String(err), true);
    }
  }, [saveNow, setRunning, setStatus]);

  const retryInput = useCallback(async () => {
    setStatus("正在重试全局输入监听…");
    try {
      await invoke("retry_input");
      await new Promise((resolve) => window.setTimeout(resolve, 600));
      const next = await invoke<InputStatus>("input_status");
      setInputStatus(next);
      const ready = next.keyboardState === "ready" && next.mouseState === "ready";
      const installing =
        next.keyboardState === "requested" ||
        next.keyboardState === "installing" ||
        next.mouseState === "requested" ||
        next.mouseState === "installing";
      setStatus(
        ready
          ? "全局输入已就绪"
          : installing
            ? "全局输入正在安装；鼠标监听将在首次鼠标输入后确认"
            : (next.error ?? "全局输入仍不可用，请尝试以管理员身份运行"),
        !ready && !installing,
      );
    } catch (err) {
      setStatus(String(err), true);
    }
  }, [setStatus]);

  const closeApp = useCallback(async () => {
    try {
      await invoke("request_close");
    } catch (err) {
      setStatus(String(err), true);
    }
  }, [setStatus]);

  useEffect(() => {
    let alive = true;
    (async () => {
      let savedUi = { ...DEFAULT_UI };
      let savedKeys: HotkeyMap = { ...DEFAULT_HOTKEYS };
      let savedHold: ButtonCn = "左键";
      let warning: string | null = null;
      try {
        const result = await invoke<LoadResult>("load_settings");
        warning = result.warning;
        const doc = result.doc;
        if (doc) {
          let migratedUnsafeHotkey = false;
          for (const action of ACTIONS) {
            const combo = normalizeCombo(doc.hotkeys?.[action]);
            if (!combo) continue;
            const validation = validateHotkey(combo);
            if (validation.ok) savedKeys[action] = validation.combo;
            else migratedUnsafeHotkey = true;
          }
          if (migratedUnsafeHotkey) {
            warning = warning ?? "旧设置中的危险热键已恢复为安全默认值";
          }
          if (doc.hold_button === "left" || doc.hold_button === "middle" || doc.hold_button === "right") {
            savedHold = BTN_CN[doc.hold_button];
          }
          savedUi = sanitizeUi(doc.ui);
        }
      } catch {
        /* 无配置时使用默认值 */
      }
      if (!alive) return;
      uiRef.current = savedUi;
      setUiState(savedUi);
      applyHotkeys(savedKeys);
      holdButtonRef.current = savedHold;
      setHoldButtonState(savedHold);
      setLoaded(true);
      if (warning) setStatus(warning, true);
      void refreshLibrary();
      void invoke<boolean>("is_admin").then(setAdmin).catch(() => {});
      void invoke<InputStatus>("input_status").then(setInputStatus).catch(() => {});
      void invoke<TaskSnapshot>("task_status")
        .then((snapshot) => {
          if (snapshot.running) {
            runningTaskIdRef.current = snapshot.id;
            setRunning(snapshot.kind);
            setProgress(snapshot.stopping ? "后台任务正在停止" : "后台任务运行中");
          }
        })
        .catch(() => {});
    })();
    return () => {
      alive = false;
    };
  }, [applyHotkeys, refreshLibrary, setRunning, setStatus]);

  useEffect(() => {
    if (!loaded) return;
    const timer = window.setTimeout(() => {
      void saveNow().catch(() => {});
    }, 200);
    return () => window.clearTimeout(timer);
  }, [ui, hotkeys, holdButton, loaded, saveNow]);

  useEffect(() => {
    if (!loaded) return;
    void invoke("set_hotkeys", { hotkeys }).catch((err) => {
      setStatus(`热键未生效：${String(err)}`, true);
    });
  }, [hotkeys, loaded, setStatus]);

  // 运行期每 2 秒对账一次；监听线程退出后界面能及时提示，且无鼠标输入不会被误判为损坏。
  useEffect(() => {
    if (!loaded) return;
    let alive = true;
    const refresh = async () => {
      try {
        const next = await invoke<InputStatus>("input_status");
        if (alive) setInputStatus(next);
      } catch {
        /* 状态查询失败不覆盖上次结果 */
      }
    };
    void refresh();
    const timer = window.setInterval(refresh, 2000);
    return () => {
      alive = false;
      window.clearInterval(timer);
    };
  }, [loaded]);

  useEffect(() => {
    if (!loaded) return;
    void invoke("set_topmost", { enabled: ui.top }).catch(() => {});
  }, [ui.top, loaded]);

  useEffect(() => {
    let alive = true;
    const tick = async () => {
      if (document.hidden) return;
      try {
        const pos = await invoke<[number, number]>("get_cursor");
        if (alive) {
          setCursor((prev) => (prev && prev[0] === pos[0] && prev[1] === pos[1] ? prev : pos));
        }
      } catch {
        /* 读取失败时保持上次坐标 */
      }
    };
    void tick();
    const timer = window.setInterval(tick, 200);
    return () => {
      alive = false;
      window.clearInterval(timer);
    };
  }, []);

  useEffect(() => {
    let disposed = false;
    const unlisteners: Array<() => void> = [];
    const pending = [
      listen<Record<string, never>>("close-requested", () => {
        void performClose();
      }),
      listen<{ action: Action }>("hotkey", (event) => {
        handlersRef.current[event.payload.action]?.();
      }),
      listen<{ combo: string | null }>("hotkey-captured", (event) => {
        capturedRef.current(event.payload.combo);
      }),
      listen<{ id: number; count: number }>("clicker-progress", (event) => {
        if (runningTaskIdRef.current !== event.payload.id) return;
        setClickCount(event.payload.count);
      }),
      listen<{ id: number; count: number; reason: string; error?: string | null }>(
        "clicker-done",
        (event) => {
          const { id, count, reason, error } = event.payload;
          if (runningTaskIdRef.current !== id) return;
          setRunning(null);
          setProgress(null);
          setClickCount(count);
          if (reason === "corner") {
            setStatus("已触发角落急停，连点停止", true);
          } else if (reason === "error") {
            setStatus(`连点中断：${error ?? "光标定位失败"}`, true);
          } else {
            setStatus(`连点结束，共点击 ${count} 次`);
          }
        },
      ),
      listen<{ count: number }>("record-progress", (event) => {
        if (runningRef.current !== "record") return;
        setProgress(`已记录 ${event.payload.count} 个事件`);
      }),
      listen<{ count: number; max: number }>("record-limit", (event) => {
        if (runningRef.current !== "record") return;
        setProgress(null);
        setStatus(
          `录制达到上限（${event.payload.count}/${event.payload.max}），请停止并保存`,
          true,
        );
      }),
      listen<{ id: number; loop: number; loops: number }>("play-progress", (event) => {
        if (runningTaskIdRef.current !== event.payload.id) return;
        const { loop, loops } = event.payload;
        setProgress(`第 ${loop}${loops ? `/${loops}` : ""} 遍`);
      }),
      listen<{ id: number; reason: string; error?: string | null }>("play-done", (event) => {
        const { id, reason, error } = event.payload;
        if (runningTaskIdRef.current !== id) return;
        setRunning(null);
        setProgress(null);
        if (reason === "corner") {
          setStatus("已触发角落急停，回放停止", true);
        } else if (reason === "error") {
          setStatus(`回放中断：${error ?? "未知错误"}`, true);
        } else {
          setStatus("回放结束");
        }
      }),
      listen<{ id: number; reason: string }>("hold-stopped", (event) => {
        if (runningTaskIdRef.current !== event.payload.id) return;
        setRunning(null);
        setProgress(null);
        beep("stop");
        setStatus("长按已松开");
      }),
    ];
    void Promise.all(pending)
      .then((fns) => {
        if (disposed) fns.forEach((fn) => fn());
        else unlisteners.push(...fns);
      })
      .catch((err) => setStatus(`界面事件订阅失败：${String(err)}`, true));
    return () => {
      disposed = true;
      unlisteners.forEach((fn) => fn());
    };
  }, [beep, performClose, setRunning, setStatus]);

  // 任务事件可能丢失或乱序；以任务 ID + 后端快照做低频最终对账。
  useEffect(() => {
    if (!loaded) return;
    let alive = true;
    const reconcile = async () => {
      try {
        const snapshot = await invoke<TaskSnapshot>("task_status");
        if (!alive) return;
        if (snapshot.running) {
          if (
            runningRef.current !== snapshot.kind ||
            runningTaskIdRef.current !== snapshot.id
          ) {
            runningTaskIdRef.current = snapshot.id;
            setRunning(snapshot.kind);
            setProgress(snapshot.stopping ? "后台任务正在停止" : "后台任务运行中");
          }
        } else if (runningRef.current !== null) {
          setRunning(null);
          setProgress(null);
          setStatus("后台任务已结束");
        }
      } catch {
        /* 对账失败保留当前界面状态，下一轮再试 */
      }
    };
    const timer = window.setInterval(reconcile, 1000);
    return () => {
      alive = false;
      window.clearInterval(timer);
    };
  }, [loaded, setRunning, setStatus]);

  useEffect(() => {
    if (!dialog) return;
    const previous = document.activeElement as HTMLElement | null;
    const node = modalRef.current;
    const initial = node?.querySelector<HTMLElement>("input, button") ?? node;
    initial?.focus();
    const onKey = (event: KeyboardEvent) => {
      if (event.key === "Escape") {
        event.preventDefault();
        cancelDialog();
      } else if (event.key === "Enter" && dialog.kind === "rename") {
        event.preventDefault();
        void confirmDialog();
      } else if (event.key === "Tab" && node) {
        const focusable = Array.from(
          node.querySelectorAll<HTMLElement>('button, input, [tabindex]:not([tabindex="-1"])'),
        );
        if (!focusable.length) return;
        const first = focusable[0];
        const last = focusable[focusable.length - 1];
        if (event.shiftKey && document.activeElement === first) {
          event.preventDefault();
          last.focus();
        } else if (!event.shiftKey && document.activeElement === last) {
          event.preventDefault();
          first.focus();
        }
      }
    };
    window.addEventListener("keydown", onKey);
    return () => {
      window.removeEventListener("keydown", onKey);
      previous?.focus?.();
    };
  }, [cancelDialog, confirmDialog, dialog]);

  const clampNumericField = useCallback(
    (
      field: "interval" | "count" | "loops" | "pos_x" | "pos_y",
      min: number,
      max: number,
      label: string,
      allowEmpty = false,
    ) => {
      const text = uiRef.current[field].trim();
      if (!text && allowEmpty) return;
      const value = toInt(text);
      if (value === null) {
        setStatus(`${label}必须是整数`, true);
        return;
      }
      if (value < min || value > max) {
        applyUi({ [field]: String(clamp(value, min, max)) } as Partial<UiSettings>);
        setStatus(`${label}已限制在 ${min}–${max}`, true);
      }
    },
    [applyUi, setStatus],
  );

  const busy = running !== null;
  const recordLocked = busy && running !== "record";
  const playLocked = busy && running !== "play";
  const clickerLocked = busy && running !== "clicker";
  const holdLocked = busy && running !== "hold";
  const trimNote = eventTotal > MAX_ROWS ? `（仅显示前 ${MAX_ROWS} 条）` : "";
  const libraryPageStart = libraryTotal ? libraryOffset + 1 : 0;
  const libraryPageEnd = libraryOffset + library.length;
  const inputBroken =
    inputStatus &&
    (["failed", "stopped"].includes(inputStatus.keyboardState) ||
      ["failed", "stopped"].includes(inputStatus.mouseState));
  const inputInstalling =
    inputStatus &&
    (["requested", "installing"].includes(inputStatus.keyboardState) ||
      ["requested", "installing"].includes(inputStatus.mouseState));

  return (
    <div className="app" aria-busy={busy}>
      <div className="titlebar" data-tauri-drag-region>
        <span className="name" data-tauri-drag-region>
          {APP_NAME}
        </span>
        <span className="version" data-tauri-drag-region>
          {VERSION}
        </span>
        <span className="spacer" data-tauri-drag-region />
        <button
          className="win-btn"
          type="button"
          title="最小化"
          onClick={() => void getCurrentWindow().minimize().catch(() => {})}
        >
          —
        </button>
        <button className="win-btn close" type="button" title="关闭" onClick={() => void closeApp()}>
          ✕
        </button>
      </div>

      <div className="main">
        <div className="left">
          <Card title="连点设置" accent="var(--color-accent)" className="settings-card">
            <div className="settings-body">
              <div className="setting-row">
                <span className="field-label">间隔</span>
                <input
                  className="input center"
                  aria-label="点击间隔（毫秒）"
                  style={{ flex: 1 }}
                  value={ui.interval}
                  onChange={(event) => applyUi({ interval: event.target.value })}
                  onBlur={() =>
                    clampNumericField(
                      "interval",
                      LIMITS.intervalMin,
                      LIMITS.intervalMax,
                      "点击间隔",
                    )
                  }
                />
                <span className="unit">毫秒</span>
              </div>

              <div className="setting-row">
                <span className="field-label">按键</span>
                <Segmented
                  options={["左键", "中键", "右键"]}
                  value={ui.button}
                  onChange={(value) => applyUi({ button: value as ButtonCn })}
                />
              </div>

              <div className="setting-row">
                <span className="field-label">方式</span>
                <Segmented
                  options={["单击", "双击"]}
                  value={ui.double}
                  onChange={(value) => applyUi({ double: value as "单击" | "双击" })}
                />
              </div>

              <div className="setting-row">
                <span className="field-label">抖动</span>
                <Segmented
                  options={JITTER_STEPS}
                  value={ui.jitter}
                  onChange={(jitter) => applyUi({ jitter })}
                />
                <span className="unit">±%</span>
                <span className="hint" title="间隔随机化，模拟人手">随机抖动</span>
              </div>

              <div className="setting-row">
                <span className="field-label">位置</span>
                <label className="radio">
                  <input
                    type="radio"
                    name="pos-mode"
                    checked={ui.pos_mode === 0}
                    onChange={() => applyUi({ pos_mode: 0 })}
                  />
                  跟随光标
                </label>
                <label className="radio" style={{ marginLeft: 10 }}>
                  <input
                    type="radio"
                    name="pos-mode"
                    checked={ui.pos_mode === 1}
                    onChange={() => applyUi({ pos_mode: 1 })}
                  />
                  指定坐标
                </label>
              </div>

              <div className="setting-row coord-row">
                <input
                  className="input center coord-input"
                  placeholder="X"
                  aria-label="目标 X 坐标"
                  disabled={ui.pos_mode === 0}
                  value={ui.pos_x}
                  onChange={(event) => applyUi({ pos_x: event.target.value })}
                  onBlur={() =>
                    clampNumericField("pos_x", LIMITS.coordMin, LIMITS.coordMax, "X 坐标", true)
                  }
                />
                <input
                  className="input center coord-input"
                  placeholder="Y"
                  aria-label="目标 Y 坐标"
                  disabled={ui.pos_mode === 0}
                  value={ui.pos_y}
                  onChange={(event) => applyUi({ pos_y: event.target.value })}
                  onBlur={() =>
                    clampNumericField("pos_y", LIMITS.coordMin, LIMITS.coordMax, "Y 坐标", true)
                  }
                />
                <button
                  type="button"
                  className="btn primary"
                  style={{ width: 84 }}
                  disabled={busy || counting || ui.pos_mode === 0}
                  onClick={() => void pickPosition()}
                >
                  拾取坐标
                </button>
              </div>

              <div className="setting-row">
                <span className="field-label">次数</span>
                <label className="radio">
                  <input
                    type="radio"
                    name="cnt-mode"
                    checked={ui.cnt_mode === 0}
                    onChange={() => applyUi({ cnt_mode: 0 })}
                  />
                  无限
                </label>
                <label className="radio" style={{ marginLeft: 10 }}>
                  <input
                    type="radio"
                    name="cnt-mode"
                    checked={ui.cnt_mode === 1}
                    onChange={() => applyUi({ cnt_mode: 1 })}
                  />
                  指定
                </label>
                <input
                  className="input center"
                  aria-label="点击次数"
                  style={{ width: 80, marginLeft: 8 }}
                  disabled={ui.cnt_mode === 0}
                  value={ui.count}
                  onChange={(event) => applyUi({ count: event.target.value })}
                  onBlur={() =>
                    clampNumericField("count", 1, LIMITS.clicksMax, "点击次数")
                  }
                />
                <span className="unit">次</span>
              </div>

              <div className="setting-row">
                <span className="field-label">热键</span>
                <button
                  type="button"
                  className={`btn outline hotkey${capture === "clicker" ? " capturing" : ""}`}
                  onKeyUp={blockHotkeyActivation}
                  onClick={() => void startCapture("clicker")}
                >
                  {capture === "clicker" ? "按下组合键…" : comboText(hotkeys.clicker)}
                </button>
                <span className="hint" title="点击后按组合键，如 Ctrl+F6">点击后按组合键</span>
              </div>
            </div>
          </Card>

          <button
            type="button"
            className={`start-btn${running === "clicker" ? " running" : ""}`}
            disabled={clickerLocked}
            onClick={() => void toggleClicker()}
          >
            {running === "clicker" ? "■  停止连点" : "●  开始连点"}
          </button>
          <div className="count-line">已点击 {clickCount} 次</div>

          <Card title="鼠标长按" accent="var(--color-status-warning)" className="hold-card">
            <div className="hold-body">
              <div className="setting-row">
                <span className="field-label">按键</span>
                <Segmented
                  options={["左键", "中键", "右键"]}
                  value={holdButton}
                  onChange={(value) => void changeHoldButton(value as ButtonCn)}
                />
                <span className="hint" title="保持按住，配合移动可拖拽">可配合移动拖拽</span>
              </div>
              <div className="setting-row">
                <span className="field-label">热键</span>
                <button
                  type="button"
                  className={`btn outline hotkey${capture === "hold" ? " capturing" : ""}`}
                  onKeyUp={blockHotkeyActivation}
                  onClick={() => void startCapture("hold")}
                >
                  {capture === "hold" ? "按下组合键…" : comboText(hotkeys.hold)}
                </button>
                <span className="hint">按热键开始 / 再按松开</span>
              </div>
              <div className="setting-row">
                <span className="field-label">操作</span>
                <button
                  type="button"
                  className={`btn ${running === "hold" ? "danger" : "primary"} hold-action`}
                  disabled={holdLocked}
                  aria-pressed={running === "hold"}
                  onClick={() => void toggleHold()}
                >
                  {running === "hold" ? `松开${holdButton}` : `按住${holdButton}`}
                </button>
                <span className="hint" title="不依赖全局热键，仍可从这里停止">不依赖全局热键</span>
              </div>
            </div>
          </Card>
        </div>

        <div className="right">
          <Card title="录制 / 回放" accent="var(--color-status-success)" className="record-card">
            <div className="card-body">
              <div className="record-row">
                <button
                  type="button"
                  className={`btn record-btn${running === "record" ? " danger" : " primary"}`}
                  disabled={recordLocked}
                  onClick={() => void toggleRecord(true)}
                >
                  {running === "record" ? "■  停止录制" : "●  开始录制"}
                </button>
                <button
                  type="button"
                  className={`btn outline hotkey${capture === "record" ? " capturing" : ""}`}
                  onKeyUp={blockHotkeyActivation}
                  style={{ width: 96 }}
                  onClick={() => void startCapture("record")}
                >
                  {capture === "record" ? "按下组合键…" : comboText(hotkeys.record)}
                </button>
                <label className="check">
                  <input
                    type="checkbox"
                    checked={ui.moves}
                    onChange={(event) => applyUi({ moves: event.target.checked })}
                  />
                  记录鼠标移动
                </label>
                <span className="hint right-hint">录制时请勿点击本窗口</span>
              </div>
              <div className="record-row">
                <Segmented
                  options={SPEED_STEPS}
                  value={ui.speed}
                  onChange={(speed) => applyUi({ speed })}
                />
                <span className="unit" style={{ marginLeft: 4 }}>
                  循环
                </span>
                <input
                  className="input center"
                  aria-label="回放循环次数（0 为无限）"
                  style={{ width: 64, marginLeft: 6 }}
                  value={ui.loops}
                  onChange={(event) => applyUi({ loops: event.target.value })}
                  onBlur={() =>
                    clampNumericField("loops", 0, LIMITS.loopsMax, "循环次数")
                  }
                />
                <span className="unit">遍（0=无限）</span>
                <span className="spacer" />
                <button
                  type="button"
                  className={`btn outline hotkey${capture === "play" ? " capturing" : ""}`}
                  onKeyUp={blockHotkeyActivation}
                  style={{ width: 96 }}
                  onClick={() => void startCapture("play")}
                >
                  {capture === "play" ? "按下组合键…" : comboText(hotkeys.play)}
                </button>
                <button
                  type="button"
                  className={`btn play-btn${running === "play" ? " danger" : " success"}`}
                  disabled={playLocked}
                  onClick={() => void togglePlay()}
                >
                  {running === "play" ? "■  停止回放" : "▶  回放"}
                </button>
              </div>
              {pendingSave ? (
                <div className="save-alert" role="alert">
                  <span>
                    录制保存失败：{pendingSave.error}（{pendingSave.eventCount} 个事件仍在内存）
                  </span>
                  <button type="button" className="btn outline small" onClick={() => void retryPendingSave()}>
                    重试保存
                  </button>
                  <button type="button" className="btn outline small" onClick={() => void savePendingAs()}>
                    另存为
                  </button>
                  <button type="button" className="btn danger small" onClick={() => void discardPending()}>
                    放弃
                  </button>
                </div>
              ) : null}
            </div>
          </Card>

          <Card
            title="事件列表"
            accent="var(--color-accent)"
            className="events-card"
            headRight={
              <>
                <span className="card-trailing">
                  共 {eventTotal} 个事件{trimNote}
                </span>
                <button type="button" className="btn outline small" onClick={clearEvents}>
                  清空
                </button>
              </>
            }
          >
            <div className="table-area">
              <div className="table-scroll">
                <table>
                  <thead>
                    <tr>
                      <th style={{ width: 44 }}>#</th>
                      <th style={{ width: 84 }}>延时(ms)</th>
                      <th style={{ width: 110 }}>动作</th>
                      <th>坐标/参数</th>
                    </tr>
                  </thead>
                  <tbody>
                    {events.slice(0, MAX_ROWS).map((ev, index) => (
                      <EventRow key={index} ev={ev} index={index} />
                    ))}
                  </tbody>
                </table>
              </div>
              {eventTotal === 0 ? (
                <div className="empty-state">
                  还没有事件
                  <br />
                  录制或加载脚本后，每一步会列在这里
                </div>
              ) : null}
            </div>
          </Card>

          <Card
            title="脚本库"
            accent="var(--color-text-muted)"
            className="library-card"
            headRight={
              <span className="card-trailing">
                {libraryTotal ? `${libraryTotal} 个脚本` : ""}
                {activeLibraryFile ? ` · 活动：${activeLibraryFile.slice(0, -5)}` : ""}
              </span>
            }
          >
            <div className="table-area">
              <div className="table-scroll">
                <table>
                  <thead>
                    <tr>
                      <th className="left" style={{ paddingLeft: 10 }}>名称</th>
                      <th style={{ width: 76 }}>事件数</th>
                      <th style={{ width: 128 }}>保存时间</th>
                      <th style={{ width: 68 }}>状态</th>
                    </tr>
                  </thead>
                  <tbody>
                    {library.map((row) => {
                      const isActive = activeLibraryFile === row.fileName;
                      return (
                        <tr
                          key={row.fileName}
                          className={[
                            selected === row.fileName ? "selected" : "",
                            isActive ? "active" : "",
                            row.available ? "" : "unavailable",
                          ]
                            .filter(Boolean)
                            .join(" ")}
                          title={row.reason ?? row.name}
                          tabIndex={0}
                          aria-selected={selected === row.fileName}
                          aria-disabled={!row.available}
                          onClick={() => setSelected(row.fileName)}
                          onDoubleClick={() => {
                            if (row.available) void loadScript(row.fileName);
                          }}
                          onKeyDown={(event) => {
                            if (event.key === "Enter" || event.key === " ") {
                              event.preventDefault();
                              setSelected(row.fileName);
                              if (row.available) void loadScript(row.fileName);
                              else setStatus(row.reason ?? "脚本不可用", true);
                            } else if (event.key === "Delete") {
                              event.preventDefault();
                              setSelected(row.fileName);
                              setDialog({ kind: "delete", file: row.fileName, name: row.name });
                            }
                          }}
                        >
                          <td className="left name-cell">
                            {isActive ? <span className="active-dot" aria-label="当前活动脚本" /> : null}
                            {row.name}
                          </td>
                          <td>{row.available ? row.count : "—"}</td>
                          <td>{row.saved}</td>
                          <td className={row.available ? "available-cell" : "unavailable-cell"}>
                            {row.available ? "可用" : "不可用"}
                          </td>
                        </tr>
                      );
                    })}
                  </tbody>
                </table>
              </div>
              {libraryTotal === 0 ? (
                <div className="empty-state">
                  暂无脚本
                  <br />
                  录制完成后会自动按时间命名存到这里
                </div>
              ) : null}
            </div>
            <div className="lib-actions">
              <button type="button" className="btn primary" style={{ width: 72 }} onClick={loadSelected}>
                加载
              </button>
              <button type="button" className="btn outline" style={{ width: 76 }} onClick={openRename}>
                重命名
              </button>
              <button type="button" className="btn outline" style={{ width: 64 }} onClick={openDelete}>
                删除
              </button>
              <button type="button" className="btn outline" style={{ width: 64 }} onClick={() => void openTrash()}>
                回收站
              </button>
              <button
                type="button"
                className="btn outline"
                style={{ width: 56 }}
                onClick={() => void refreshLibrary(undefined, libraryOffset, true)}
              >
                刷新
              </button>
              <button
                type="button"
                className="btn outline"
                style={{ width: 92 }}
                onClick={() => void openFolder()}
              >
                打开文件夹
              </button>
              <button
                type="button"
                className="btn outline small"
                disabled={libraryOffset === 0}
                onClick={() => void refreshLibrary(undefined, Math.max(0, libraryOffset - LIBRARY_PAGE_SIZE))}
              >
                上一页
              </button>
              <span className="page-label">
                {libraryPageStart}–{libraryPageEnd} / {libraryTotal}
              </span>
              <button
                type="button"
                className="btn outline small"
                disabled={libraryPageEnd >= libraryTotal}
                onClick={() => void refreshLibrary(undefined, libraryOffset + LIBRARY_PAGE_SIZE)}
              >
                下一页
              </button>
            </div>
          </Card>
        </div>
      </div>

      <div className="statusbar">
        <span
          className={`status-text${status.warn ? " warn" : ""}`}
          role="status"
          aria-live={status.warn ? "assertive" : "polite"}
          aria-atomic="true"
          title={status.text}
        >
          {status.text}
        </span>
        {progress ? (
          <span className="task-progress" aria-hidden="true">
            {progress}
          </span>
        ) : null}
        {admin === true ? <span className="admin-ok">已以管理员身份运行</span> : null}
        {admin === false ? (
          <button
            type="button"
            className="admin-link"
            disabled={busy}
            onClick={() => void elevate()}
          >
            ⬆ 获取管理员权限
          </button>
        ) : null}
        {inputBroken ? (
          <button
            type="button"
            className="admin-link warn-link"
            title={inputStatus?.error ?? undefined}
            onClick={() => void retryInput()}
          >
            ⚠ 全局输入监听已失效，点击重试
          </button>
        ) : inputInstalling ? (
          <span className="input-pending" title={inputStatus?.error ?? undefined}>
            全局输入安装中
          </span>
        ) : null}
        <span className="cursor">
          光标: {cursor ? `(${cursor[0]}, ${cursor[1]})` : "(-, -)"}
        </span>
        <Switch checked={ui.sound} label="提示音" onChange={(sound) => applyUi({ sound })} />
        <Switch checked={ui.top} label="置顶" onChange={(top) => applyUi({ top })} />
      </div>

      {dialog ? (
        <div
          className="overlay"
          onMouseDown={(event) => {
            if (event.target === event.currentTarget) cancelDialog();
          }}
        >
          <div
            className="modal"
            role="dialog"
            aria-modal="true"
            aria-labelledby="dialog-title"
            tabIndex={-1}
            ref={modalRef}
          >
            <h3 id="dialog-title">
              {dialog.kind === "rename"
                ? "重命名脚本"
                : dialog.kind === "delete"
                  ? "移到回收站"
                  : dialog.kind === "unsaved-recording"
                    ? "录制尚未保存"
                    : dialog.kind === "settings-save"
                      ? "设置尚未保存"
                      : dialog.kind === "close-error"
                        ? "无法安全退出"
                        : "脚本回收站"}
            </h3>

            {dialog.kind === "rename" ? (
              <>
                <p>新的脚本名称：</p>
                <input
                  autoFocus
                  className="input"
                  aria-label="脚本名称"
                  style={{ width: "100%" }}
                  value={renameValue}
                  onChange={(event) => setRenameValue(event.target.value)}
                />
              </>
            ) : null}

            {dialog.kind === "delete" ? (
              <p>将「{dialog.name}」移到应用回收站。脚本文件不会立即永久删除，可稍后恢复。</p>
            ) : null}

            {dialog.kind === "unsaved-recording" ? (
              <>
                <p>
                  {pendingSave?.eventCount ?? 0} 个录制事件仍保存在内存中，尚未写入磁盘。
                  {pendingSave?.error ? ` 原因：${pendingSave.error}` : ""}
                </p>
                <p>可以重试原位置、另存到其他目录，或明确放弃这份录制。</p>
              </>
            ) : null}

            {dialog.kind === "settings-save" ? (
              <p>最新设置保存失败：{dialog.error}。可以重试、取消退出，或明确选择不保存设置直接退出。</p>
            ) : null}

            {dialog.kind === "close-error" ? <p>{dialog.message}</p> : null}

            {dialog.kind === "trash" ? (
              dialog.entries.length ? (
                <div className="trash-list">
                  {dialog.entries.map((entry) => (
                    <div className="trash-row" key={entry.id}>
                      <div>
                        <strong>{entry.name}</strong>
                        <span>{entry.deleted}</span>
                      </div>
                      <button
                        type="button"
                        className="btn outline small"
                        onClick={() => void restoreTrash(entry)}
                      >
                        恢复
                      </button>
                    </div>
                  ))}
                </div>
              ) : (
                <p>回收站为空。</p>
              )
            ) : null}

            <div className="modal-actions">
              {dialog.kind === "trash" ? (
                <button type="button" className="btn primary" onClick={() => setDialog(null)}>
                  关闭
                </button>
              ) : dialog.kind === "unsaved-recording" ? (
                <>
                  <button type="button" className="btn outline" onClick={cancelDialog}>
                    取消
                  </button>
                  <button type="button" className="btn danger" onClick={() => void discardPending()}>
                    放弃录制
                  </button>
                  <button type="button" className="btn outline" onClick={() => void savePendingAs()}>
                    另存为
                  </button>
                  <button type="button" className="btn primary" onClick={() => void retryPendingSave()}>
                    重试保存
                  </button>
                </>
              ) : dialog.kind === "settings-save" ? (
                <>
                  <button type="button" className="btn outline" onClick={cancelDialog}>
                    取消退出
                  </button>
                  <button type="button" className="btn danger" onClick={exitWithoutSettings}>
                    不保存设置并退出
                  </button>
                  <button type="button" className="btn primary" onClick={retryClose}>
                    重试
                  </button>
                </>
              ) : dialog.kind === "close-error" ? (
                <>
                  <button type="button" className="btn outline" onClick={cancelDialog}>
                    取消退出
                  </button>
                  <button type="button" className="btn primary" onClick={retryClose}>
                    重试
                  </button>
                </>
              ) : (
                <>
                  <button type="button" className="btn outline" onClick={cancelDialog}>
                    取消
                  </button>
                  <button
                    type="button"
                    className={`btn ${dialog.kind === "delete" ? "danger" : "primary"}`}
                    onClick={() => void confirmDialog()}
                  >
                    {dialog.kind === "delete" ? "移到回收站" : "确定"}
                  </button>
                </>
              )}
            </div>
          </div>
        </div>
      ) : null}
    </div>
  );
}
