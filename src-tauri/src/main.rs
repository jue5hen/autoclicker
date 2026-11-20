#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use std::collections::{HashMap, HashSet};
use std::fs;
use std::io::Write;
use std::os::raw::c_int;
use std::path::{Path, PathBuf};
use std::ptr;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::{Duration, Instant, SystemTime};

use rand::Rng;
use rdev::{Button, EventType};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tauri::{AppHandle, Emitter, Manager, State};

use winapi::shared::minwindef::{BOOL, DWORD, LPARAM, LPVOID, LRESULT, UINT, WPARAM};
use winapi::shared::windef::{HDC, HMONITOR, POINT, RECT};
use winapi::um::commdlg::{
    GetSaveFileNameW, OFN_EXPLORER, OFN_OVERWRITEPROMPT, OFN_PATHMUSTEXIST, OPENFILENAMEW,
};
use winapi::um::errhandlingapi::{GetLastError, SetLastError};
use winapi::um::handleapi::CloseHandle;
use winapi::um::processthreadsapi::{GetCurrentProcess, OpenProcessToken};
use winapi::um::securitybaseapi::GetTokenInformation;
use winapi::um::shellapi::ShellExecuteW;
use winapi::um::synchapi::{CreateEventW, CreateMutexW, OpenEventW, SetEvent, WaitForSingleObject};
use winapi::um::timeapi::{timeBeginPeriod, timeEndPeriod};
use winapi::um::utilapiset::Beep;
use winapi::um::winnt::{
    TokenElevation, EVENT_MODIFY_STATE, HANDLE, LPCWSTR, TOKEN_ELEVATION, TOKEN_QUERY,
};
use winapi::um::winuser::{
    mouse_event, CallNextHookEx, DispatchMessageA, EnumDisplayMonitors, GetCursorPos, GetMessageA,
    GetMonitorInfoW, MessageBoxW, MonitorFromPoint, SetCursorPos, SetWindowsHookExA,
    TranslateMessage, UnhookWindowsHookEx, KBDLLHOOKSTRUCT, MB_ICONINFORMATION, MB_OK, MONITORINFO,
    MONITOR_DEFAULTTOPRIMARY, MOUSEEVENTF_HWHEEL, MOUSEEVENTF_LEFTDOWN, MOUSEEVENTF_LEFTUP,
    MOUSEEVENTF_MIDDLEDOWN, MOUSEEVENTF_MIDDLEUP, MOUSEEVENTF_RIGHTDOWN, MOUSEEVENTF_RIGHTUP,
    MOUSEEVENTF_WHEEL, MSG, SW_SHOWNORMAL, WH_KEYBOARD_LL, WM_KEYDOWN, WM_KEYUP, WM_SYSKEYDOWN,
    WM_SYSKEYUP,
};

const APP_NAME: &str = "AutoClicker";
const PANIC_RANGE: i32 = 2;
const MOVE_MIN_GAP: Duration = Duration::from_millis(40);
const SCRIPT_BAD: &str = "<>:\"/\\|?*";
const MAX_EVENT_DELAY_MS: i64 = 3_600_000;
const LIBRARY_PAGE_MAX: usize = 500;
const TASK_STOP_TIMEOUT: Duration = Duration::from_secs(5);
const HANDOFF_WAIT: Duration = Duration::from_secs(10);
const HANDOFF_READY_TIMEOUT_MS: u32 = 15_000;

/// 单个脚本允许的最大事件数，防止加载超大文件拖垮 UI。
const MAX_EVENTS: usize = 500_000;
/// 录制期间在内存中保留的事件上限，避免高频输入导致内存无界增长。
const MAX_RECORD_EVENTS: usize = 100_000;
/// 单个脚本文件的大小上限（32 MB）。
const MAX_SCRIPT_BYTES: u64 = 32 * 1024 * 1024;
const MAX_SETTINGS_BYTES: u64 = 256 * 1024;
const MAX_CLICKS: u64 = 999_999_999;
const MAX_INTERVAL_MS: u64 = 3_600_000;
const MAX_LOOPS: u64 = 1_000_000;
const SCRIPT_PAGE_MAX: usize = 5_000;
const SINGLE_INSTANCE_NAME: &str = "Local\\AutoClicker_SingleInstance";
const ERROR_ALREADY_EXISTS_CODE: i32 = 183;

const HOTKEY_ACTIONS: [&str; 4] = ["clicker", "record", "play", "hold"];
const MOD_ORDER: [&str; 4] = ["ctrl", "alt", "shift", "cmd"];
const SYSTEM_RESERVED_HOTKEYS: [&str; 6] = [
    "alt+f4",
    "alt+tab",
    "alt+esc",
    "ctrl+esc",
    "ctrl+shift+esc",
    "ctrl+alt+delete",
];
/// 单次按住超过这个时长就视为抬键事件丢失，接受下一次按下为新按键。
const KEY_HELD_GUARD: Duration = Duration::from_secs(30);

fn kind_cn(kind: TaskKind) -> &'static str {
    match kind {
        TaskKind::Clicker => "连点",
        TaskKind::Record => "录制",
        TaskKind::Play => "回放",
        TaskKind::Hold => "长按",
    }
}

fn base_dir() -> PathBuf {
    std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(Path::to_path_buf))
        .unwrap_or_else(|| PathBuf::from("."))
}

fn settings_path() -> PathBuf {
    base_dir().join("settings.json")
}

fn scripts_dir() -> PathBuf {
    base_dir().join("scripts")
}

// ---------------------------------------------------------------- 输入注入

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
enum Btn {
    Left,
    Middle,
    Right,
}

impl Btn {
    fn parse(name: &str) -> Result<Btn, String> {
        match name {
            "left" => Ok(Btn::Left),
            "middle" => Ok(Btn::Middle),
            "right" => Ok(Btn::Right),
            _ => Err(format!("未知的鼠标按键：{name}")),
        }
    }

    fn down_flag(self) -> DWORD {
        match self {
            Btn::Left => MOUSEEVENTF_LEFTDOWN,
            Btn::Middle => MOUSEEVENTF_MIDDLEDOWN,
            Btn::Right => MOUSEEVENTF_RIGHTDOWN,
        }
    }

    fn up_flag(self) -> DWORD {
        match self {
            Btn::Left => MOUSEEVENTF_LEFTUP,
            Btn::Middle => MOUSEEVENTF_MIDDLEUP,
            Btn::Right => MOUSEEVENTF_RIGHTUP,
        }
    }
}

fn mouse_send(flags: DWORD, data: DWORD) {
    unsafe {
        mouse_event(flags, 0, 0, data, 0);
    }
}

fn click_button(btn: Btn, double: bool) {
    for _ in 0..if double { 2 } else { 1 } {
        mouse_send(btn.down_flag(), 0);
        mouse_send(btn.up_flag(), 0);
    }
}

fn try_cursor_pos() -> Result<(i32, i32), String> {
    unsafe {
        let mut p = POINT { x: 0, y: 0 };
        if GetCursorPos(&mut p) != 0 {
            Ok((p.x, p.y))
        } else {
            Err("无法读取当前光标位置".into())
        }
    }
}

fn rect_tuple(rect: RECT) -> (i32, i32, i32, i32) {
    (rect.left, rect.top, rect.right, rect.bottom)
}

fn point_in_rect_i32(x: i32, y: i32, rect: (i32, i32, i32, i32)) -> bool {
    x >= rect.0 && x < rect.2 && y >= rect.1 && y < rect.3
}

struct MonitorSearch {
    x: i32,
    y: i32,
    found: bool,
}

unsafe extern "system" fn monitor_search_proc(
    monitor: HMONITOR,
    _hdc: HDC,
    _clip: *mut RECT,
    data: LPARAM,
) -> BOOL {
    let search = &mut *(data as *mut MonitorSearch);
    let mut info: MONITORINFO = std::mem::zeroed();
    info.cbSize = std::mem::size_of::<MONITORINFO>() as DWORD;
    if GetMonitorInfoW(monitor, &mut info) != 0
        && point_in_rect_i32(search.x, search.y, rect_tuple(info.rcMonitor))
    {
        search.found = true;
        return 0;
    }
    1
}

/// 坐标必须落在真实显示器的矩形内；虚拟桌面包围盒中的显示器空洞不算合法位置。
fn point_on_monitor(x: i32, y: i32) -> bool {
    let mut search = MonitorSearch { x, y, found: false };
    unsafe {
        EnumDisplayMonitors(
            ptr::null_mut(),
            ptr::null(),
            Some(monitor_search_proc),
            &mut search as *mut MonitorSearch as LPARAM,
        );
    }
    search.found
}

fn move_cursor_checked(x: i32, y: i32) -> Result<(), String> {
    if !point_on_monitor(x, y) {
        return Err(format!("目标坐标 ({x}, {y}) 不在当前任何显示器上"));
    }
    unsafe {
        if SetCursorPos(x, y) == 0 {
            return Err(format!("无法把光标移动到 ({x}, {y})"));
        }
    }
    let actual = try_cursor_pos()?;
    if actual != (x, y) {
        return Err(format!(
            "光标未到达目标位置：期望 ({x}, {y})，实际 ({}, {})",
            actual.0, actual.1
        ));
    }
    Ok(())
}

fn primary_monitor_rect() -> Option<(i32, i32, i32, i32)> {
    unsafe {
        let monitor = MonitorFromPoint(POINT { x: 0, y: 0 }, MONITOR_DEFAULTTOPRIMARY);
        if monitor.is_null() {
            return None;
        }
        let mut info: MONITORINFO = std::mem::zeroed();
        info.cbSize = std::mem::size_of::<MONITORINFO>() as DWORD;
        if GetMonitorInfoW(monitor, &mut info) == 0 {
            return None;
        }
        Some(rect_tuple(info.rcMonitor))
    }
}

fn panic_corner_hit(x: i32, y: i32, rect: (i32, i32, i32, i32)) -> bool {
    x >= rect.0 && x <= rect.0 + PANIC_RANGE && y >= rect.1 && y <= rect.1 + PANIC_RANGE
}

/// 急停锚定主屏左上角的 3×3 物理像素区域，不会把负坐标副屏误判为角落。
fn at_panic_corner() -> bool {
    let Ok((x, y)) = try_cursor_pos() else {
        return false;
    };
    primary_monitor_rect().is_some_and(|rect| panic_corner_hit(x, y, rect))
}

struct Precision;

impl Precision {
    fn new() -> Self {
        unsafe {
            timeBeginPeriod(1);
        }
        Precision
    }
}

impl Drop for Precision {
    fn drop(&mut self) {
        unsafe {
            timeEndPeriod(1);
        }
    }
}

/// 统一的停止条件：显式停止，或用户把鼠标甩到左上角触发急停。
/// 每个睡眠切片都检查一次，保证长间隔下急停也能在 20ms 级别响应。
fn stop_hit(stop: &AtomicBool) -> bool {
    stop.load(Ordering::SeqCst) || at_panic_corner()
}

fn kind_key(kind: TaskKind) -> &'static str {
    match kind {
        TaskKind::Clicker => "clicker",
        TaskKind::Record => "record",
        TaskKind::Play => "play",
        TaskKind::Hold => "hold",
    }
}

fn stop_reason(stop: &AtomicBool) -> &'static str {
    if stop.load(Ordering::SeqCst) {
        "stopped"
    } else {
        "corner"
    }
}

/// 等到绝对截止时间而不是“每次执行后再睡固定时长”，避免连点/回放逐渐漂移。
fn stoppable_wait_until(stop: &AtomicBool, deadline: Instant) -> bool {
    loop {
        if stop_hit(stop) {
            return false;
        }
        let now = Instant::now();
        if now >= deadline {
            return true;
        }
        thread::sleep((deadline - now).min(Duration::from_millis(20)));
    }
}

fn next_deadline(previous: Instant, delay: Duration) -> Instant {
    previous.checked_add(delay).unwrap_or_else(Instant::now)
}

// ---------------------------------------------------------------- 任务状态

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum TaskKind {
    Clicker,
    Record,
    Play,
    Hold,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
enum InputHealth {
    Unknown,
    Requested,
    Installing,
    Ready,
    Failed,
    Stopped,
}

fn input_needs_retry(state: InputHealth) -> bool {
    matches!(
        state,
        InputHealth::Unknown | InputHealth::Failed | InputHealth::Stopped
    )
}

struct TaskCompletion {
    done: Mutex<bool>,
    cv: Condvar,
}

struct TaskSlot {
    kind: TaskKind,
    stop: Arc<AtomicBool>,
    id: u64,
    done: Arc<TaskCompletion>,
}

#[derive(Clone, Copy)]
struct HoldState {
    id: u64,
    btn: Btn,
}

struct PendingRecording {
    events: Vec<ScriptEvent>,
    overflowed: bool,
    error: String,
}

#[derive(Serialize, Clone)]
#[serde(rename_all = "camelCase")]
struct PendingRecordingInfo {
    event_count: usize,
    overflowed: bool,
    error: String,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct SavedRecording {
    saved_name: String,
    event_count: usize,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ClosePreparation {
    ready: bool,
    pending_recording: Option<PendingRecordingInfo>,
    error: Option<String>,
}

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct InputStatus {
    keyboard: bool,
    mouse: bool,
    keyboard_state: InputHealth,
    mouse_state: InputHealth,
    error: Option<String>,
}

struct Core {
    task: Mutex<Option<TaskSlot>>,
    next_id: AtomicU64,
    hotkey_map: Mutex<HashMap<String, String>>,
    capture: AtomicBool,
    capture_mute: Mutex<Option<Instant>>,
    mods: Mutex<HashSet<String>>,
    keys_held: Mutex<HashMap<String, Instant>>,
    recording: AtomicBool,
    rec_moves: AtomicBool,
    rec_events: Mutex<Vec<ScriptEvent>>,
    rec_last: Mutex<Option<Instant>>,
    rec_emit: Mutex<Instant>,
    rec_overflow: AtomicBool,
    rec_overflow_reported: AtomicBool,
    rec_window: Mutex<Option<(i32, i32, i32, i32)>>,
    hold_button: Mutex<Option<HoldState>>,
    input_status: Mutex<InputStatus>,
    hook_installing: AtomicBool,
    listener_started: AtomicBool,
    settings_write: Mutex<()>,
    script_meta: Mutex<HashMap<PathBuf, CachedScriptMeta>>,
    loaded_script: Mutex<Option<LoadedScript>>,
    script_pages: Mutex<HashMap<String, ScriptLibrarySnapshot>>,
    next_script_page: AtomicU64,
    pending_recording: Mutex<Option<PendingRecording>>,
    closing: AtomicBool,
    close_ready: AtomicBool,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct TaskSnapshot {
    running: bool,
    kind: Option<&'static str>,
    id: Option<u64>,
    stopping: bool,
}

impl Core {
    fn new() -> Self {
        Core {
            task: Mutex::new(None),
            next_id: AtomicU64::new(0),
            hotkey_map: Mutex::new(HashMap::new()),
            capture: AtomicBool::new(false),
            capture_mute: Mutex::new(None),
            mods: Mutex::new(HashSet::new()),
            keys_held: Mutex::new(HashMap::new()),
            recording: AtomicBool::new(false),
            rec_moves: AtomicBool::new(false),
            rec_events: Mutex::new(Vec::new()),
            rec_last: Mutex::new(None),
            rec_emit: Mutex::new(Instant::now() - Duration::from_secs(1)),
            rec_overflow: AtomicBool::new(false),
            rec_overflow_reported: AtomicBool::new(false),
            rec_window: Mutex::new(None),
            hold_button: Mutex::new(None),
            input_status: Mutex::new(InputStatus {
                keyboard: false,
                mouse: false,
                keyboard_state: InputHealth::Unknown,
                mouse_state: InputHealth::Unknown,
                error: None,
            }),
            hook_installing: AtomicBool::new(false),
            listener_started: AtomicBool::new(false),
            settings_write: Mutex::new(()),
            script_meta: Mutex::new(HashMap::new()),
            loaded_script: Mutex::new(None),
            script_pages: Mutex::new(HashMap::new()),
            next_script_page: AtomicU64::new(0),
            pending_recording: Mutex::new(None),
            closing: AtomicBool::new(false),
            close_ready: AtomicBool::new(false),
        }
    }

    fn ensure_open(&self) -> Result<(), String> {
        if self.closing.load(Ordering::SeqCst) {
            Err("程序正在退出，请稍候".into())
        } else {
            Ok(())
        }
    }

    fn ensure_no_pending_recording(&self, action: &str) -> Result<(), String> {
        let pending = self
            .pending_recording
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        match pending.as_ref() {
            Some(item) => Err(format!(
                "{action}前请先保存或放弃 {} 个未落盘录制事件",
                item.events.len()
            )),
            None => Ok(()),
        }
    }

    fn reserve(
        &self,
        kind: TaskKind,
    ) -> Result<(u64, Arc<AtomicBool>, Arc<TaskCompletion>), String> {
        self.ensure_open()?;
        let mut slot = self.task.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(current) = slot.as_ref() {
            return Err(format!("{}正在进行", kind_cn(current.kind)));
        }
        let id = self.next_id.fetch_add(1, Ordering::SeqCst) + 1;
        let stop = Arc::new(AtomicBool::new(false));
        let done = Arc::new(TaskCompletion {
            done: Mutex::new(false),
            cv: Condvar::new(),
        });
        *slot = Some(TaskSlot {
            kind,
            stop: stop.clone(),
            id,
            done: done.clone(),
        });
        Ok((id, stop, done))
    }

    /// 只有拥有任务槽的工作线程完成输入释放后，才允许清槽并唤醒等待者。
    fn complete(&self, id: u64) -> bool {
        let taken = {
            let mut slot = self.task.lock().unwrap_or_else(|e| e.into_inner());
            if slot.as_ref().map(|t| t.id) != Some(id) {
                return false;
            }
            slot.take()
        };
        if let Some(task) = taken {
            task.stop.store(true, Ordering::SeqCst);
            let mut done = task.done.done.lock().unwrap_or_else(|e| e.into_inner());
            *done = true;
            task.done.cv.notify_all();
            true
        } else {
            false
        }
    }

    fn task_id(&self, kind: TaskKind) -> Option<u64> {
        self.task
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
            .filter(|t| t.kind == kind)
            .map(|t| t.id)
    }

    fn snapshot(&self) -> TaskSnapshot {
        let slot = self.task.lock().unwrap_or_else(|e| e.into_inner());
        match slot.as_ref() {
            Some(task) => TaskSnapshot {
                running: true,
                kind: Some(kind_key(task.kind)),
                id: Some(task.id),
                stopping: task.stop.load(Ordering::SeqCst),
            },
            None => TaskSnapshot {
                running: false,
                kind: None,
                id: None,
                stopping: false,
            },
        }
    }

    fn stop_kind(&self, kind: TaskKind) -> bool {
        if let Some(slot) = self.task.lock().unwrap_or_else(|e| e.into_inner()).as_ref() {
            if slot.kind == kind {
                slot.stop.store(true, Ordering::SeqCst);
                return true;
            }
        }
        false
    }

    fn request_stop_all(&self) {
        if let Some(slot) = self.task.lock().unwrap_or_else(|e| e.into_inner()).as_ref() {
            slot.stop.store(true, Ordering::SeqCst);
        }
    }

    fn wait_task(&self, id: u64, timeout: Duration) -> bool {
        let done = {
            let slot = self.task.lock().unwrap_or_else(|e| e.into_inner());
            match slot.as_ref() {
                None => return true,
                Some(task) if task.id == id => task.done.clone(),
                Some(_) => return false,
            }
        };
        let guard = done.done.lock().unwrap_or_else(|e| e.into_inner());
        let (guard, _) = done
            .cv
            .wait_timeout_while(guard, timeout, |finished| !*finished)
            .unwrap_or_else(|e| e.into_inner());
        *guard
    }

    fn wait_idle(&self, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        loop {
            let done = {
                let slot = self.task.lock().unwrap_or_else(|e| e.into_inner());
                match slot.as_ref() {
                    None => return true,
                    Some(task) => task.done.clone(),
                }
            };
            let now = Instant::now();
            if now >= deadline {
                return false;
            }
            let remaining = deadline - now;
            let guard = done.done.lock().unwrap_or_else(|e| e.into_inner());
            let (guard, wait) = done
                .cv
                .wait_timeout_while(guard, remaining, |finished| !*finished)
                .unwrap_or_else(|e| e.into_inner());
            if *guard || wait.timed_out() {
                return self
                    .task
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .is_none();
            }
        }
    }
}

/// 任务槽的 RAII 守卫：线程无论正常结束还是 panic 展开，都会在输入释放后释放槽位。
struct TaskGuard {
    core: Arc<Core>,
    id: u64,
}

impl Drop for TaskGuard {
    fn drop(&mut self) {
        self.core.complete(self.id);
    }
}

/// 长按线程的兜底：只有当前代际仍拥有 hold_button 时才发送抬起。
struct HoldGuard {
    core: Arc<Core>,
    id: u64,
}

fn take_hold_for_id(state: &mut Option<HoldState>, id: u64) -> Option<Btn> {
    if state.as_ref().is_some_and(|hold| hold.id == id) {
        state.take().map(|hold| hold.btn)
    } else {
        None
    }
}

fn release_hold_owned(core: &Arc<Core>, id: u64) -> bool {
    let mut state = core.hold_button.lock().unwrap_or_else(|e| e.into_inner());
    let Some(btn) = take_hold_for_id(&mut state, id) else {
        return false;
    };
    // down/up 与所有权变更共用同一把锁，旧守卫不能释放新一轮长按。
    mouse_send(btn.up_flag(), 0);
    true
}

impl Drop for HoldGuard {
    fn drop(&mut self) {
        release_hold_owned(&self.core, self.id);
        self.core.complete(self.id);
    }
}

/// 回放期间跟踪已按下的键，Drop 时统一释放。
#[derive(Default)]
struct PressedButtons(HashSet<Btn>);

impl PressedButtons {
    fn press(&mut self, btn: Btn) {
        self.0.insert(btn);
    }

    fn release(&mut self, btn: Btn) {
        self.0.remove(&btn);
    }

    fn release_all(&mut self) {
        for btn in self.0.drain() {
            mouse_send(btn.up_flag(), 0);
        }
    }
}

impl Drop for PressedButtons {
    fn drop(&mut self) {
        self.release_all();
    }
}

// ---------------------------------------------------------------- 日志 / 输入状态

fn log_path() -> PathBuf {
    base_dir().join("logs").join("app.log")
}

/// 追加一行日志。release 没有控制台，出问题时只能靠这里留痕。
fn append_log(line: &str) {
    let path = log_path();
    if let Some(dir) = path.parent() {
        let _ = fs::create_dir_all(dir);
    }
    if fs::metadata(&path).map(|m| m.len()).unwrap_or(0) > 2 * 1024 * 1024 {
        let rotated = path.with_extension("log.1");
        let _ = fs::remove_file(&rotated);
        let _ = fs::rename(&path, rotated);
    }
    if let Ok(mut f) = fs::OpenOptions::new().create(true).append(true).open(&path) {
        let now = chrono::Local::now().format("%Y-%m-%d %H:%M:%S");
        let _ = writeln!(f, "[{now}] {line}");
    }
}

fn set_input_status(
    core: &Arc<Core>,
    keyboard: Option<InputHealth>,
    mouse: Option<InputHealth>,
    error: Option<String>,
) {
    let mut status = core.input_status.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(state) = keyboard {
        status.keyboard_state = state;
        status.keyboard = state == InputHealth::Ready;
    }
    if let Some(state) = mouse {
        status.mouse_state = state;
        status.mouse = state == InputHealth::Ready;
    }
    if let Some(err) = error {
        status.error = Some(err);
    }
    if status.keyboard_state == InputHealth::Ready && status.mouse_state == InputHealth::Ready {
        status.error = None;
    }
}

fn mark_mouse_ready(core: &Arc<Core>) {
    let mut status = core.input_status.lock().unwrap_or_else(|e| e.into_inner());
    if status.mouse_state != InputHealth::Ready {
        status.mouse_state = InputHealth::Ready;
        status.mouse = true;
        if status.keyboard_state == InputHealth::Ready {
            status.error = None;
        }
    }
}

fn install_keyboard_hook(core: Arc<Core>) {
    if core.hook_installing.swap(true, Ordering::SeqCst) {
        return;
    }
    set_input_status(&core, Some(InputHealth::Requested), None, None);
    thread::spawn(move || unsafe {
        set_input_status(&core, Some(InputHealth::Installing), None, None);
        let hook = SetWindowsHookExA(WH_KEYBOARD_LL, Some(keyboard_hook), ptr::null_mut(), 0);
        if hook.is_null() {
            core.hook_installing.store(false, Ordering::SeqCst);
            let err = std::io::Error::last_os_error();
            append_log(&format!("keyboard hook install failed: {err}"));
            set_input_status(
                &core,
                Some(InputHealth::Failed),
                None,
                Some(format!("键盘钩子安装失败：{err}")),
            );
            return;
        }
        set_input_status(&core, Some(InputHealth::Ready), None, None);
        append_log("keyboard hook installed");
        let mut msg: MSG = std::mem::zeroed();
        // 钩子线程必须持续泵消息，否则 Windows 会跳过这个钩子。
        loop {
            let rc = GetMessageA(&mut msg, ptr::null_mut(), 0, 0);
            if rc <= 0 {
                append_log(&format!("keyboard hook pump exited rc={rc}"));
                break;
            }
            TranslateMessage(&msg);
            DispatchMessageA(&msg);
        }
        UnhookWindowsHookEx(hook);
        core.hook_installing.store(false, Ordering::SeqCst);
        set_input_status(
            &core,
            Some(InputHealth::Stopped),
            None,
            Some("键盘钩子已退出，请点击重试".into()),
        );
    });
}

static INSTANCE_HANDLE: AtomicUsize = AtomicUsize::new(0);

fn instance_name() -> String {
    if std::env::var_os("AUTOCLICKER_BACKGROUND_TEST").is_some() {
        format!("Local\\AutoClicker_BackgroundTest_{}", std::process::id())
    } else {
        SINGLE_INSTANCE_NAME.to_string()
    }
}

fn handoff_requested() -> bool {
    std::env::args().any(|arg| arg == "--elevated-handoff" || arg.starts_with("--handoff-ready="))
}

fn handoff_ready_name() -> Option<String> {
    std::env::args().find_map(|arg| arg.strip_prefix("--handoff-ready=").map(str::to_string))
}

fn release_instance_handle() {
    let raw = INSTANCE_HANDLE.swap(0, Ordering::SeqCst);
    if raw != 0 {
        unsafe {
            CloseHandle(raw as HANDLE);
        }
    }
}

fn reacquire_instance_handle() -> bool {
    let name = wide(&instance_name());
    unsafe {
        SetLastError(0);
        let handle = CreateMutexW(ptr::null_mut(), 0, name.as_ptr());
        if handle.is_null() || GetLastError() == ERROR_ALREADY_EXISTS_CODE as u32 {
            if !handle.is_null() {
                CloseHandle(handle);
            }
            return false;
        }
        INSTANCE_HANDLE.store(handle as usize, Ordering::SeqCst);
        true
    }
}

fn signal_handoff_ready() {
    let Some(name) = handoff_ready_name() else {
        return;
    };
    let name = wide(&name);
    unsafe {
        let event = OpenEventW(EVENT_MODIFY_STATE, 0, name.as_ptr());
        if !event.is_null() {
            SetEvent(event);
            CloseHandle(event);
        }
    }
}

fn single_instance_guard() -> bool {
    let name = wide(&instance_name());
    let handoff = handoff_requested();
    let deadline = Instant::now() + HANDOFF_WAIT;
    loop {
        unsafe {
            SetLastError(0);
            let handle = CreateMutexW(ptr::null_mut(), 0, name.as_ptr());
            if handle.is_null() {
                append_log("single instance mutex creation failed");
                return false;
            }
            if GetLastError() != ERROR_ALREADY_EXISTS_CODE as u32 {
                INSTANCE_HANDLE.store(handle as usize, Ordering::SeqCst);
                return true;
            }
            CloseHandle(handle);
        }
        if handoff && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(100));
            continue;
        }
        let text = wide("AutoClicker 已在运行，请勿重复启动。");
        let caption = wide(APP_NAME);
        unsafe {
            MessageBoxW(
                ptr::null_mut(),
                text.as_ptr(),
                caption.as_ptr(),
                MB_OK | MB_ICONINFORMATION,
            );
        }
        return false;
    }
}

fn is_main_key(key: &str) -> bool {
    if key.chars().count() == 1 {
        return key.chars().all(|c| {
            c.is_ascii_lowercase()
                || c.is_ascii_digit()
                || matches!(
                    c,
                    '`' | '-' | '=' | '[' | ']' | '\\' | ';' | '\'' | ',' | '.' | '/'
                )
        });
    }
    matches!(
        key,
        "esc"
            | "space"
            | "enter"
            | "tab"
            | "backspace"
            | "delete"
            | "home"
            | "end"
            | "pageup"
            | "pagedown"
            | "left"
            | "up"
            | "right"
            | "down"
    ) || key
        .strip_prefix('f')
        .is_some_and(|rest| rest.parse::<u8>().is_ok_and(|n| (1..=12).contains(&n)))
}

fn normalize_combo(raw: &str) -> Option<String> {
    let mut mods: Vec<&str> = Vec::new();
    let mut main: Option<&str> = None;
    let lower = raw.trim().to_lowercase();
    for part in lower.split('+').map(str::trim) {
        if part.is_empty() {
            return None;
        }
        if MOD_ORDER.contains(&part) {
            if mods.contains(&part) {
                return None;
            }
            mods.push(part);
        } else if is_main_key(part) {
            if main.is_some() {
                return None;
            }
            main = Some(part);
        } else {
            return None;
        }
    }
    let main = main?;
    let mut parts: Vec<&str> = MOD_ORDER
        .iter()
        .copied()
        .filter(|m| mods.contains(m))
        .collect();
    parts.push(main);
    Some(parts.join("+"))
}

fn hotkey_policy_error(combo: &str) -> Option<String> {
    let parts = combo.split('+').collect::<Vec<_>>();
    let main = *parts.last()?;
    let mods = &parts[..parts.len() - 1];
    if mods.is_empty()
        && !main
            .strip_prefix('f')
            .is_some_and(|n| n.parse::<u8>().is_ok_and(|value| (1..=12).contains(&value)))
    {
        return Some("裸键容易误触，请至少添加 Ctrl、Alt 或 Shift".into());
    }
    if mods.contains(&"cmd") {
        return Some("不支持 Win 组合键，系统可能优先截获".into());
    }
    if SYSTEM_RESERVED_HOTKEYS.contains(&combo) {
        return Some("该组合键属于 Windows 系统保留动作，不能可靠用作全局热键".into());
    }
    None
}

fn is_reserved_name(name: &str) -> bool {
    let upper = name.trim_end_matches('.').to_uppercase();
    let stem = upper.split('.').next().unwrap_or("");
    matches!(stem, "CON" | "PRN" | "AUX" | "NUL")
        || (stem.len() == 4
            && (stem.starts_with("COM") || stem.starts_with("LPT"))
            && stem.as_bytes()[3].is_ascii_digit()
            && stem.as_bytes()[3] != b'0')
}

// ---------------------------------------------------------------- 提示音

fn spawn_beep(kind: &str, sound: bool) {
    if !sound {
        return;
    }
    let tone: (u32, u32) = match kind {
        "start" => (880, 90),
        "stop" => (520, 110),
        "tick" => (660, 60),
        "done" => (784, 150),
        _ => return,
    };
    thread::spawn(move || unsafe {
        Beep(tone.0, tone.1);
    });
}

#[tauri::command]
fn beep(kind: String, sound: bool) {
    spawn_beep(&kind, sound);
}

// ---------------------------------------------------------------- 连点

#[tauri::command]
#[allow(clippy::too_many_arguments)]
fn start_clicker(
    app: AppHandle,
    core: State<'_, Arc<Core>>,
    interval: u64,
    button: String,
    double: bool,
    position: Option<[i32; 2]>,
    limit: u64,
    jitter: f64,
) -> Result<u64, String> {
    core.ensure_open()?;
    if let Some(slot) = core.task.lock().unwrap_or_else(|e| e.into_inner()).as_ref() {
        return Err(format!("无法开始连点：{}正在进行", kind_cn(slot.kind)));
    }
    let btn = Btn::parse(&button)?;
    if !(10..=MAX_INTERVAL_MS).contains(&interval) {
        return Err(format!("点击间隔需在 10–{MAX_INTERVAL_MS} 毫秒之间"));
    }
    if !jitter.is_finite() || !(0.0..=0.5).contains(&jitter) {
        return Err("抖动参数需在 0–50% 之间".into());
    }
    if limit > MAX_CLICKS {
        return Err(format!("点击次数不能超过 {MAX_CLICKS}"));
    }
    if let Some([x, y]) = position {
        if !(-100_000..=100_000).contains(&x) || !(-100_000..=100_000).contains(&y) {
            return Err("目标坐标超出允许范围".into());
        }
        if !point_on_monitor(x, y) {
            return Err(format!("目标坐标 ({x}, {y}) 不在当前任何显示器上"));
        }
    }
    let (id, stop, _done) = core.reserve(TaskKind::Clicker)?;
    let core = core.inner().clone();
    let worker_core = core.clone();
    thread::Builder::new()
        .name("auto-clicker".into())
        .spawn(move || {
            let _guard = TaskGuard {
                core: worker_core,
                id,
            };
            let _precision = Precision::new();
            let mut count: u64 = 0;
            let mut last_emit = Instant::now() - Duration::from_secs(1);
            let mut next = Instant::now();
            let mut error: Option<String> = None;
            let reason = loop {
                if stop.load(Ordering::SeqCst) {
                    break "stopped";
                }
                if at_panic_corner() {
                    break "corner";
                }
                let move_result = match position {
                    Some([x, y]) => move_cursor_checked(x, y),
                    None => try_cursor_pos().and_then(|(x, y)| {
                        if point_on_monitor(x, y) {
                            Ok(())
                        } else {
                            Err("当前光标不在任何显示器上".to_string())
                        }
                    }),
                };
                if let Err(err) = move_result {
                    error = Some(err);
                    break "error";
                }
                click_button(btn, double);
                count += 1;
                if last_emit.elapsed() >= Duration::from_millis(100) {
                    last_emit = Instant::now();
                    let _ = app.emit("clicker-progress", json!({ "id": id, "count": count }));
                }
                if limit > 0 && count >= limit {
                    break "done";
                }
                let mut wait = interval as f64;
                if jitter > 0.0 {
                    let factor = rand::thread_rng().gen_range(-jitter..=jitter);
                    wait *= 1.0 + factor;
                }
                let delay = Duration::from_secs_f64(wait.max(1.0) / 1000.0);
                let scheduled = next_deadline(next, delay);
                next = if scheduled < Instant::now() {
                    Instant::now() + delay
                } else {
                    scheduled
                };
                if !stoppable_wait_until(&stop, next) {
                    break stop_reason(&stop);
                }
            };
            let _ = app.emit("clicker-progress", json!({ "id": id, "count": count }));
            let _ = app.emit(
                "clicker-done",
                json!({ "id": id, "count": count, "reason": reason, "error": error }),
            );
        })
        .map_err(|err| {
            core.complete(id);
            format!("无法启动连点线程：{err}")
        })?;
    Ok(id)
}

#[tauri::command]
fn stop_clicker(core: State<'_, Arc<Core>>) -> bool {
    core.stop_kind(TaskKind::Clicker)
}

// ---------------------------------------------------------------- 长按

#[tauri::command]
fn start_hold(app: AppHandle, core: State<'_, Arc<Core>>, button: String) -> Result<u64, String> {
    core.ensure_open()?;
    if let Some(slot) = core.task.lock().unwrap_or_else(|e| e.into_inner()).as_ref() {
        return Err(format!("无法开始长按：{}正在进行", kind_cn(slot.kind)));
    }
    let btn = Btn::parse(&button)?;
    let (id, stop, _done) = core.reserve(TaskKind::Hold)?;
    {
        let mut hold = core.hold_button.lock().unwrap_or_else(|e| e.into_inner());
        if hold.is_some() {
            drop(hold);
            core.complete(id);
            return Err("长按状态异常：仍有未释放的鼠标键".into());
        }
        *hold = Some(HoldState { id, btn });
        // 所有权写入与实际按下在同一把锁内完成。
        mouse_send(btn.down_flag(), 0);
    }
    let app2 = app.clone();
    let core2 = core.inner().clone();
    let worker_core = core2.clone();
    thread::Builder::new()
        .name("auto-clicker-hold".into())
        .spawn(move || {
            let guard = HoldGuard {
                core: worker_core,
                id,
            };
            let reason = loop {
                if stop.load(Ordering::SeqCst) {
                    break "stopped";
                }
                if at_panic_corner() {
                    break "corner";
                }
                thread::sleep(Duration::from_millis(20));
            };
            drop(guard);
            let _ = app2.emit("hold-stopped", json!({ "id": id, "reason": reason }));
        })
        .map_err(|err| {
            release_hold_owned(core.inner(), id);
            core.complete(id);
            format!("无法启动长按线程：{err}")
        })?;
    Ok(id)
}

#[tauri::command]
fn stop_hold(_app: AppHandle, core: State<'_, Arc<Core>>) -> Result<bool, String> {
    let Some(id) = core.task_id(TaskKind::Hold) else {
        return Ok(false);
    };
    core.stop_kind(TaskKind::Hold);
    if !core.wait_task(id, Duration::from_secs(2)) {
        return Err("长按释放超时，请重试".into());
    }
    Ok(true)
}

// ---------------------------------------------------------------- 回放

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScriptEvent {
    #[serde(rename = "type")]
    pub ev_type: String,
    #[serde(default)]
    pub dt: i64,
    pub x: i64,
    pub y: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub button: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pressed: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dx: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dy: Option<i64>,
}

impl ScriptEvent {
    fn valid(&self) -> bool {
        if !(-100_000..=100_000).contains(&self.x) || !(-100_000..=100_000).contains(&self.y) {
            return false;
        }
        if !(0..=MAX_EVENT_DELAY_MS).contains(&self.dt) {
            return false;
        }
        match self.ev_type.as_str() {
            "click" => {
                matches!(
                    self.button.as_deref(),
                    Some("left") | Some("middle") | Some("right")
                ) && self.pressed.is_some()
            }
            "move" | "wait" => true,
            "scroll" => {
                self.dx.is_some_and(|v| (-1_000..=1_000).contains(&v))
                    && self.dy.is_some_and(|v| (-1_000..=1_000).contains(&v))
            }
            _ => false,
        }
    }
}

fn execute_event(ev: &ScriptEvent, pressed: &mut PressedButtons) -> Result<(), String> {
    match ev.ev_type.as_str() {
        "wait" => Ok(()),
        "move" => move_cursor_checked(ev.x as i32, ev.y as i32),
        "click" => {
            move_cursor_checked(ev.x as i32, ev.y as i32)?;
            let btn = Btn::parse(ev.button.as_deref().unwrap_or("left"))?;
            if ev.pressed.unwrap_or(false) {
                mouse_send(btn.down_flag(), 0);
                pressed.press(btn);
            } else {
                mouse_send(btn.up_flag(), 0);
                pressed.release(btn);
            }
            Ok(())
        }
        "scroll" => {
            // 滚轮同样按录制坐标定位，纯滚轮脚本不会落到当前光标所在窗口。
            move_cursor_checked(ev.x as i32, ev.y as i32)?;
            let dx = ev.dx.unwrap_or(0).clamp(-1_000, 1_000);
            let dy = ev.dy.unwrap_or(0).clamp(-1_000, 1_000);
            if dx != 0 {
                mouse_send(MOUSEEVENTF_HWHEEL, (dx * 120) as u32);
            }
            if dy != 0 {
                mouse_send(MOUSEEVENTF_WHEEL, (dy * 120) as u32);
            }
            Ok(())
        }
        other => Err(format!("未知事件类型：{other}")),
    }
}

fn spawn_playback(
    app: AppHandle,
    core: Arc<Core>,
    events: Arc<Vec<ScriptEvent>>,
    speed: f64,
    loops: u64,
) -> Result<u64, String> {
    core.ensure_open()?;
    if let Some(slot) = core.task.lock().unwrap_or_else(|e| e.into_inner()).as_ref() {
        return Err(format!("无法开始回放：{}正在进行", kind_cn(slot.kind)));
    }
    if events.is_empty() {
        return Err("没有可回放的事件，请先录制或在「脚本库」加载".into());
    }
    if events.len() > MAX_EVENTS {
        return Err(format!("没有可回放的事件：单次最多 {MAX_EVENTS} 个"));
    }
    if loops > MAX_LOOPS {
        return Err(format!("回放循环次数不能超过 {MAX_LOOPS}"));
    }
    if !speed.is_finite() || speed <= 0.0 {
        return Err("回放速度无效".into());
    }
    if !events.iter().all(ScriptEvent::valid) {
        return Err("没有可回放的事件，请先录制或在「脚本库」加载".into());
    }
    let speed = speed.max(0.1);
    let (id, stop, _done) = core.reserve(TaskKind::Play)?;
    let worker_core = core.clone();
    thread::Builder::new()
        .name("auto-clicker-play".into())
        .spawn(move || {
            let _guard = TaskGuard {
                core: worker_core,
                id,
            };
            let _precision = Precision::new();
            let mut reason = "done";
            let mut error: Option<String> = None;
            let mut pressed = PressedButtons::default();
            let mut loop_now: u64 = 0;
            let mut next = Instant::now();
            'outer: loop {
                if loops > 0 && loop_now >= loops {
                    break;
                }
                loop_now += 1;
                let _ = app.emit(
                    "play-progress",
                    json!({ "id": id, "loop": loop_now, "loops": loops }),
                );
                for ev in events.iter() {
                    if stop.load(Ordering::SeqCst) {
                        reason = "stopped";
                        break 'outer;
                    }
                    if at_panic_corner() {
                        reason = "corner";
                        break 'outer;
                    }
                    let delay = Duration::from_secs_f64(ev.dt.max(0) as f64 / 1000.0 / speed);
                    if !delay.is_zero() {
                        next = next_deadline(next, delay);
                        if !stoppable_wait_until(&stop, next) {
                            reason = stop_reason(&stop);
                            break 'outer;
                        }
                    }
                    if stop.load(Ordering::SeqCst) {
                        reason = "stopped";
                        break 'outer;
                    }
                    if at_panic_corner() {
                        reason = "corner";
                        break 'outer;
                    }
                    if let Err(err) = execute_event(ev, &mut pressed) {
                        reason = "error";
                        error = Some(err);
                        break 'outer;
                    }
                }
                // 单个循环不平衡（缺少抬起）时先释放，避免下一轮重复按下或卡键。
                pressed.release_all();
            }
            // 先释放所有仍按下的键，再发完成事件；TaskGuard 在事件发出后才清槽。
            pressed.release_all();
            let _ = app.emit(
                "play-done",
                json!({ "id": id, "reason": reason, "error": error }),
            );
        })
        .map_err(|err| {
            core.complete(id);
            format!("无法启动回放线程：{err}")
        })?;
    Ok(id)
}

#[tauri::command]
fn start_playback(
    app: AppHandle,
    core: State<'_, Arc<Core>>,
    events: Vec<ScriptEvent>,
    speed: f64,
    loops: u64,
) -> Result<u64, String> {
    spawn_playback(app, core.inner().clone(), Arc::new(events), speed, loops)
}

#[tauri::command]
fn stop_playback(core: State<'_, Arc<Core>>) -> bool {
    core.stop_kind(TaskKind::Play)
}

#[tauri::command]
fn task_status(core: State<'_, Arc<Core>>) -> TaskSnapshot {
    core.snapshot()
}

// ---------------------------------------------------------------- 录制

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct RecordResult {
    events: Vec<ScriptEvent>,
    saved_name: Option<String>,
    save_error: Option<String>,
    overflowed: bool,
    pending_save: bool,
}

fn now_ms(last: &mut Option<Instant>) -> i64 {
    let now = Instant::now();
    let dt = match *last {
        Some(prev) => now.duration_since(prev).as_millis() as i64,
        None => 0,
    };
    *last = Some(now);
    dt
}

fn main_window_rect(app: &AppHandle) -> Option<(i32, i32, i32, i32)> {
    let window = app.get_webview_window("main")?;
    let pos = window.outer_position().ok()?;
    let size = window.outer_size().ok()?;
    Some((
        pos.x,
        pos.y,
        pos.x + size.width as i32,
        pos.y + size.height as i32,
    ))
}

fn point_in_rect(x: i64, y: i64, rect: (i32, i32, i32, i32)) -> bool {
    x >= rect.0 as i64 && x < rect.2 as i64 && y >= rect.1 as i64 && y < rect.3 as i64
}

fn split_long_delay(event: ScriptEvent) -> Vec<ScriptEvent> {
    if event.dt <= MAX_EVENT_DELAY_MS {
        return vec![event];
    }
    let mut remaining = event.dt;
    let mut parts = Vec::new();
    while remaining > MAX_EVENT_DELAY_MS {
        parts.push(ScriptEvent {
            ev_type: "wait".into(),
            dt: MAX_EVENT_DELAY_MS,
            x: event.x,
            y: event.y,
            button: None,
            pressed: None,
            dx: None,
            dy: None,
        });
        remaining -= MAX_EVENT_DELAY_MS;
    }
    let mut tail = event;
    tail.dt = remaining;
    parts.push(tail);
    parts
}

fn record_event(app: &AppHandle, core: &Arc<Core>, event: ScriptEvent) {
    if core
        .rec_window
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .is_some_and(|rect| point_in_rect(event.x, event.y, rect))
    {
        // 录制时忽略本窗口内的点击，尤其是“停止录制”按钮本身。
        return;
    }
    let parts = split_long_delay(event);
    let mut events = core.rec_events.lock().unwrap_or_else(|e| e.into_inner());
    let mut overflow = false;
    for part in parts {
        if events.len() >= MAX_RECORD_EVENTS {
            overflow = true;
            break;
        }
        events.push(part);
    }
    let count = events.len();
    drop(events);
    if overflow {
        core.rec_overflow.store(true, Ordering::SeqCst);
        if !core.rec_overflow_reported.swap(true, Ordering::SeqCst) {
            let _ = app.emit(
                "record-limit",
                json!({ "count": MAX_RECORD_EVENTS, "max": MAX_RECORD_EVENTS }),
            );
        }
    }
    let mut emit = core.rec_emit.lock().unwrap_or_else(|e| e.into_inner());
    if emit.elapsed() >= Duration::from_millis(100) {
        *emit = Instant::now();
        let _ = app.emit("record-progress", json!({ "count": count }));
    }
}

fn record_click(app: &AppHandle, core: &Arc<Core>, btn: Button, pressed: bool) {
    if !core.recording.load(Ordering::SeqCst) {
        return;
    }
    let name = match btn {
        Button::Left => "left",
        Button::Middle => "middle",
        Button::Right => "right",
        _ => return,
    };
    let Ok((x, y)) = try_cursor_pos() else {
        return;
    };
    let mut last = core.rec_last.lock().unwrap_or_else(|e| e.into_inner());
    let dt = now_ms(&mut last);
    drop(last);
    record_event(
        app,
        core,
        ScriptEvent {
            ev_type: "click".into(),
            dt,
            x: x as i64,
            y: y as i64,
            button: Some(name.into()),
            pressed: Some(pressed),
            dx: None,
            dy: None,
        },
    );
}

fn wheel_steps(v: i64) -> i64 {
    if v.abs() >= 120 {
        v / 120
    } else {
        v
    }
}

fn record_scroll(app: &AppHandle, core: &Arc<Core>, dx: i64, dy: i64) {
    if !core.recording.load(Ordering::SeqCst) {
        return;
    }
    let dx = wheel_steps(dx);
    let dy = wheel_steps(dy);
    if dx == 0 && dy == 0 {
        return;
    }
    let Ok((x, y)) = try_cursor_pos() else {
        return;
    };
    let mut last = core.rec_last.lock().unwrap_or_else(|e| e.into_inner());
    let dt = now_ms(&mut last);
    drop(last);
    record_event(
        app,
        core,
        ScriptEvent {
            ev_type: "scroll".into(),
            dt,
            x: x as i64,
            y: y as i64,
            button: None,
            pressed: None,
            dx: Some(dx),
            dy: Some(dy),
        },
    );
}

fn record_move(app: &AppHandle, core: &Arc<Core>, x: f64, y: f64) {
    if !core.recording.load(Ordering::SeqCst) || !core.rec_moves.load(Ordering::SeqCst) {
        return;
    }
    let mut last = core.rec_last.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(prev) = *last {
        if prev.elapsed() < MOVE_MIN_GAP {
            return;
        }
    }
    let dt = now_ms(&mut last);
    drop(last);
    record_event(
        app,
        core,
        ScriptEvent {
            ev_type: "move".into(),
            dt,
            x: x.round() as i64,
            y: y.round() as i64,
            button: None,
            pressed: None,
            dx: None,
            dy: None,
        },
    );
}

#[tauri::command]
fn start_recording(app: AppHandle, core: State<'_, Arc<Core>>, moves: bool) -> Result<u64, String> {
    core.ensure_open()?;
    core.ensure_no_pending_recording("开始新录制")?;
    if let Some(slot) = core.task.lock().unwrap_or_else(|e| e.into_inner()).as_ref() {
        return Err(format!("无法开始录制：{}正在进行", kind_cn(slot.kind)));
    }
    let (id, _stop, _done) = core.reserve(TaskKind::Record)?;
    let mut events = core.rec_events.lock().unwrap_or_else(|e| e.into_inner());
    events.clear();
    drop(events);
    *core.rec_last.lock().unwrap_or_else(|e| e.into_inner()) = None;
    *core.rec_emit.lock().unwrap_or_else(|e| e.into_inner()) =
        Instant::now() - Duration::from_secs(1);
    core.rec_overflow.store(false, Ordering::SeqCst);
    core.rec_overflow_reported.store(false, Ordering::SeqCst);
    *core.rec_window.lock().unwrap_or_else(|e| e.into_inner()) = main_window_rect(&app);
    core.rec_moves.store(moves, Ordering::SeqCst);
    core.recording.store(true, Ordering::SeqCst);
    Ok(id)
}

fn trim_tail(events: &mut Vec<ScriptEvent>, rect: (i32, i32, i32, i32)) {
    let (x0, y0, x1, y1) = rect;
    let mut cut = events.len();
    let mut acc: i64 = 0;
    for i in (0..events.len()).rev() {
        if acc > 250 {
            break;
        }
        let ev = &events[i];
        if (ev.x as i32) >= x0 && (ev.x as i32) < x1 && (ev.y as i32) >= y0 && (ev.y as i32) < y1 {
            cut = i;
        }
        acc += ev.dt;
    }
    events.truncate(cut);
}

fn write_atomic_script(path: &Path, text: &str) -> std::io::Result<()> {
    let seq = WRITE_SEQ.fetch_add(1, Ordering::Relaxed) + 1;
    let filename = path
        .file_name()
        .map(|name| name.to_string_lossy())
        .unwrap_or_default();
    let tmp = path.with_file_name(format!(".{filename}.{}.{}.tmp", std::process::id(), seq));
    let result = (|| {
        fs::write(&tmp, text)?;
        if let Ok(file) = fs::OpenOptions::new().write(true).read(true).open(&tmp) {
            file.sync_all()?;
        }
        if !path.exists() {
            return fs::rename(&tmp, path);
        }
        let backup = path.with_file_name(format!(
            ".{filename}.{}.{}.script.bak",
            std::process::id(),
            seq
        ));
        fs::rename(path, &backup)?;
        match fs::rename(&tmp, path) {
            Ok(()) => {
                let _ = fs::remove_file(&backup);
                Ok(())
            }
            Err(err) => {
                let _ = fs::rename(&backup, path);
                Err(err)
            }
        }
    })();
    let _ = fs::remove_file(&tmp);
    result
}

fn save_script_to_path(events: &[ScriptEvent], path: &Path) -> Result<String, String> {
    let path = if path
        .extension()
        .is_some_and(|ext| ext.to_string_lossy().eq_ignore_ascii_case("json"))
    {
        path.to_path_buf()
    } else {
        path.with_extension("json")
    };
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|_| "无法创建脚本文件夹".to_string())?;
    }
    let doc = json!({ "app": APP_NAME, "version": 1, "events": events });
    let text = serde_json::to_string_pretty(&doc).map_err(|_| "无法保存脚本".to_string())?;
    write_atomic_script(&path, &text).map_err(|_| "无法保存脚本".to_string())?;
    Ok(path
        .file_name()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_default())
}

fn save_script_events_in_dir(dir: &Path, events: &[ScriptEvent]) -> Result<String, String> {
    fs::create_dir_all(dir).map_err(|_| "无法创建脚本文件夹".to_string())?;
    let base = chrono::Local::now()
        .format("录制_%Y-%m-%d_%H-%M-%S")
        .to_string();
    let mut path = dir.join(format!("{base}.json"));
    let mut n = 2;
    while path.exists() {
        path = dir.join(format!("{base}-{n}.json"));
        n += 1;
    }
    save_script_to_path(events, &path)
}

fn save_script_events(events: &[ScriptEvent]) -> Result<String, String> {
    save_script_events_in_dir(&scripts_dir(), events)
}

fn pending_recording_info(core: &Core) -> Option<PendingRecordingInfo> {
    core.pending_recording
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .as_ref()
        .map(|item| PendingRecordingInfo {
            event_count: item.events.len(),
            overflowed: item.overflowed,
            error: item.error.clone(),
        })
}

fn store_pending_recording(core: &Core, events: Vec<ScriptEvent>, overflowed: bool, error: String) {
    *core
        .pending_recording
        .lock()
        .unwrap_or_else(|e| e.into_inner()) = Some(PendingRecording {
        events,
        overflowed,
        error,
    });
}

fn try_save_pending(core: &Core) -> Result<SavedRecording, String> {
    let mut pending = core
        .pending_recording
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let Some(item) = pending.as_mut() else {
        return Err("没有待保存的录制".into());
    };
    match save_script_events(&item.events) {
        Ok(saved_name) => {
            let result = SavedRecording {
                saved_name,
                event_count: item.events.len(),
            };
            *pending = None;
            Ok(result)
        }
        Err(err) => {
            item.error = err.clone();
            Err(err)
        }
    }
}

fn choose_script_save_path(default_name: &str) -> Result<Option<PathBuf>, String> {
    let mut buffer = vec![0u16; 32_768];
    let buffer_capacity = buffer.len() - 1;
    for (slot, unit) in buffer
        .iter_mut()
        .take(buffer_capacity)
        .zip(default_name.encode_utf16())
    {
        *slot = unit;
    }
    let filter = "AutoClicker 脚本 (*.json)\0*.json\0所有文件 (*.*)\0*.*\0\0"
        .encode_utf16()
        .collect::<Vec<_>>();
    let title = wide("另存录制脚本");
    let default_ext = wide("json");
    let mut ofn: OPENFILENAMEW = unsafe { std::mem::zeroed() };
    ofn.lStructSize = std::mem::size_of::<OPENFILENAMEW>() as DWORD;
    ofn.lpstrFilter = filter.as_ptr();
    ofn.lpstrFile = buffer.as_mut_ptr();
    ofn.nMaxFile = buffer.len() as DWORD;
    ofn.lpstrTitle = title.as_ptr();
    ofn.lpstrDefExt = default_ext.as_ptr();
    ofn.Flags = OFN_EXPLORER | OFN_PATHMUSTEXIST | OFN_OVERWRITEPROMPT;
    let ok = unsafe { GetSaveFileNameW(&mut ofn) };
    if ok == 0 {
        return Ok(None);
    }
    let len = buffer
        .iter()
        .position(|unit| *unit == 0)
        .unwrap_or(buffer.len());
    let mut path = PathBuf::from(String::from_utf16_lossy(&buffer[..len]));
    if !path
        .extension()
        .is_some_and(|ext| ext.to_string_lossy().eq_ignore_ascii_case("json"))
    {
        path.set_extension("json");
    }
    Ok(Some(path))
}

fn save_pending_as(core: &Core) -> Result<Option<SavedRecording>, String> {
    let default_name = chrono::Local::now()
        .format("录制_%Y-%m-%d_%H-%M-%S.json")
        .to_string();
    let Some(path) = choose_script_save_path(&default_name)? else {
        return Ok(None);
    };
    let mut pending = core
        .pending_recording
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let Some(item) = pending.as_mut() else {
        return Ok(None);
    };
    match save_script_to_path(&item.events, &path) {
        Ok(saved_name) => {
            let result = SavedRecording {
                saved_name,
                event_count: item.events.len(),
            };
            *pending = None;
            Ok(Some(result))
        }
        Err(err) => {
            item.error = err.clone();
            Err(err)
        }
    }
}

fn discard_pending_recording(core: &Core) {
    *core
        .pending_recording
        .lock()
        .unwrap_or_else(|e| e.into_inner()) = None;
}

fn stop_recording_with_saver<F>(
    core: &Arc<Core>,
    trim_rect: Option<(i32, i32, i32, i32)>,
    save: F,
) -> Result<RecordResult, String>
where
    F: Fn(&[ScriptEvent]) -> Result<String, String>,
{
    if !core.recording.swap(false, Ordering::SeqCst) {
        return Ok(RecordResult {
            events: vec![],
            saved_name: None,
            save_error: pending_recording_info(core).map(|item| item.error),
            overflowed: false,
            pending_save: pending_recording_info(core).is_some(),
        });
    }
    if let Some(id) = core.task_id(TaskKind::Record) {
        core.complete(id);
    }
    let overflowed = core.rec_overflow.swap(false, Ordering::SeqCst);
    *core.rec_window.lock().unwrap_or_else(|e| e.into_inner()) = None;
    let mut events =
        std::mem::take(&mut *core.rec_events.lock().unwrap_or_else(|e| e.into_inner()));
    if let Some(rect) = trim_rect {
        trim_tail(&mut events, rect);
    }
    if events.is_empty() {
        return Ok(RecordResult {
            events,
            saved_name: None,
            save_error: None,
            overflowed,
            pending_save: false,
        });
    }
    match save(&events) {
        Ok(saved_name) => Ok(RecordResult {
            events,
            saved_name: Some(saved_name),
            save_error: None,
            overflowed,
            pending_save: false,
        }),
        Err(err) => {
            // 保存失败时把完整快照留在后端，界面可重试或另存，不能销毁唯一副本。
            store_pending_recording(core, events.clone(), overflowed, err.clone());
            Ok(RecordResult {
                events,
                saved_name: None,
                save_error: Some(err),
                overflowed,
                pending_save: true,
            })
        }
    }
}

fn stop_recording_internal(
    core: &Arc<Core>,
    trim_rect: Option<(i32, i32, i32, i32)>,
) -> Result<RecordResult, String> {
    stop_recording_with_saver(core, trim_rect, save_script_events)
}

#[tauri::command]
fn stop_recording(
    app: AppHandle,
    core: State<'_, Arc<Core>>,
    trim: bool,
) -> Result<RecordResult, String> {
    let rect = if trim { main_window_rect(&app) } else { None };
    stop_recording_internal(core.inner(), rect)
}

#[tauri::command]
fn retry_pending_recording(core: State<'_, Arc<Core>>) -> Result<SavedRecording, String> {
    try_save_pending(core.inner())
}

#[tauri::command]
fn save_pending_recording_as(core: State<'_, Arc<Core>>) -> Result<Option<SavedRecording>, String> {
    save_pending_as(core.inner())
}

#[tauri::command]
fn discard_pending_recording_command(core: State<'_, Arc<Core>>) -> Result<(), String> {
    discard_pending_recording(core.inner());
    Ok(())
}

// ---------------------------------------------------------------- 全局热键

fn vk_modifier(vk: u32) -> Option<&'static str> {
    match vk {
        0x11 | 0xA2 | 0xA3 => Some("ctrl"),
        0xA0 | 0xA1 => Some("shift"),
        0x12 | 0xA4 | 0xA5 => Some("alt"),
        0x5B | 0x5C => Some("cmd"),
        _ => None,
    }
}

/// 虚拟键码到热键名称。全局键盘钩子直接拿到的是 VK，
/// 不再依赖第三方库的键位枚举，钩子失效时也只影响这一处。
fn vk_name(vk: u32) -> Option<&'static str> {
    let name = match vk {
        0x08 => "backspace",
        0x09 => "tab",
        0x0D => "enter",
        0x1B => "esc",
        0x20 => "space",
        0x21 => "pageup",
        0x22 => "pagedown",
        0x23 => "end",
        0x24 => "home",
        0x25 => "left",
        0x26 => "up",
        0x27 => "right",
        0x28 => "down",
        0x2E => "delete",
        0x70 => "f1",
        0x71 => "f2",
        0x72 => "f3",
        0x73 => "f4",
        0x74 => "f5",
        0x75 => "f6",
        0x76 => "f7",
        0x77 => "f8",
        0x78 => "f9",
        0x79 => "f10",
        0x7A => "f11",
        0x7B => "f12",
        0xC0 => "`",
        0xBD => "-",
        0xBB => "=",
        0xDB => "[",
        0xDD => "]",
        0xDC => "\\",
        0xBA => ";",
        0xDE => "'",
        0xBC => ",",
        0xBE => ".",
        0xBF => "/",
        0x30..=0x39 => {
            const DIGITS: [&str; 10] = ["0", "1", "2", "3", "4", "5", "6", "7", "8", "9"];
            return DIGITS.get((vk - 0x30) as usize).copied();
        }
        0x41..=0x5A => {
            const LETTERS: [&str; 26] = [
                "a", "b", "c", "d", "e", "f", "g", "h", "i", "j", "k", "l", "m", "n", "o", "p",
                "q", "r", "s", "t", "u", "v", "w", "x", "y", "z",
            ];
            return LETTERS.get((vk - 0x41) as usize).copied();
        }
        _ => return None,
    };
    Some(name)
}

fn combo_string(mods: &HashSet<String>, name: &str) -> String {
    let mut parts: Vec<&str> = Vec::new();
    for m in ["ctrl", "alt", "shift", "cmd"] {
        if mods.contains(m) {
            parts.push(m);
        }
    }
    parts.push(name);
    parts.join("+")
}

fn handle_key_event(app: &AppHandle, core: &Arc<Core>, vk: u32, pressed: bool) {
    if let Some(m) = vk_modifier(vk) {
        let mut mods = core.mods.lock().unwrap_or_else(|e| e.into_inner());
        if pressed {
            mods.insert(m.to_string());
        } else {
            mods.remove(m);
        }
        return;
    }
    let Some(name) = vk_name(vk) else {
        return;
    };
    if !pressed {
        core.keys_held
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(name);
        return;
    }
    {
        let mut held = core.keys_held.lock().unwrap_or_else(|e| e.into_inner());
        // 按住热键时系统会持续发送「按下」自动重复，它们不是新的按键，
        // 一律忽略；否则长按会反复开关任务。若抬起事件真的丢失，
        // KEY_HELD_GUARD 之后把下一次按下当作新按键，避免该键永久失效。
        if held
            .get(name)
            .is_some_and(|at| at.elapsed() < KEY_HELD_GUARD)
        {
            return;
        }
        held.insert(name.to_string(), Instant::now());
    }
    let mods = core.mods.lock().unwrap_or_else(|e| e.into_inner()).clone();
    let combo = combo_string(&mods, name);
    if core.capture.load(Ordering::SeqCst) {
        core.capture.store(false, Ordering::SeqCst);
        let result = if name == "esc" { None } else { Some(combo) };
        let _ = app.emit("hotkey-captured", json!({ "combo": result }));
        return;
    }
    let muted = core
        .capture_mute
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .as_ref()
        .is_some_and(|at| at.elapsed() < Duration::from_millis(900));
    if muted {
        return;
    }
    let action = core
        .hotkey_map
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(&combo)
        .cloned();
    if let Some(action) = action {
        let _ = app.emit("hotkey", json!({ "action": action }));
    }
}

/// 全局键盘钩子的回调必须极快返回：Windows 会跳过（并可能永久摘掉）
/// 超时的低级钩子，所以这里只解析键码并把「是否命中热键」的判断
/// 留在钩子线程里做最小工作，鼠标事件仍由 rdev 线程处理。
static KEY_SINK: std::sync::OnceLock<(AppHandle, Arc<Core>)> = std::sync::OnceLock::new();
static KEY_TX: std::sync::OnceLock<std::sync::mpsc::Sender<(u32, bool)>> =
    std::sync::OnceLock::new();

unsafe extern "system" fn keyboard_hook(code: c_int, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    // 钩子回调必须几乎立刻返回：Windows 会跳过（并可能长期摘掉）超时的低级钩子，
    // 那样全局热键会静默失灵。所以这里只取键码入队，判断交给工作线程。
    let msg = wparam as UINT;
    let pressed = matches!(msg, WM_KEYDOWN | WM_SYSKEYDOWN);
    let released = matches!(msg, WM_KEYUP | WM_SYSKEYUP);
    if code >= 0 && (pressed || released) {
        let data = &*(lparam as *const KBDLLHOOKSTRUCT);
        if let Some(tx) = KEY_TX.get() {
            let _ = tx.send((data.vkCode, pressed));
        }
    }
    CallNextHookEx(ptr::null_mut(), code, wparam, lparam)
}

fn spawn_keyboard_hook(app: AppHandle, core: Arc<Core>) {
    let (tx, rx) = std::sync::mpsc::channel::<(u32, bool)>();
    let _ = KEY_TX.set(tx);
    let _ = KEY_SINK.set((app, core.clone()));
    thread::spawn(move || {
        while let Ok((vk, pressed)) = rx.recv() {
            if let Some((app, core)) = KEY_SINK.get() {
                handle_key_event(app, core, vk, pressed);
            }
        }
    });
    install_keyboard_hook(core);
}

fn handle_input(app: &AppHandle, core: &Arc<Core>, event: rdev::Event) {
    match event.event_type {
        // 键盘由自己的 WH_KEYBOARD_LL 钩子处理，这里只收鼠标，避免重复触发。
        EventType::KeyPress(_) | EventType::KeyRelease(_) => {}
        EventType::ButtonPress(btn) => {
            mark_mouse_ready(core);
            record_click(app, core, btn, true);
        }
        EventType::ButtonRelease(btn) => {
            mark_mouse_ready(core);
            record_click(app, core, btn, false);
        }
        EventType::MouseMove { x, y } => {
            mark_mouse_ready(core);
            record_move(app, core, x, y);
        }
        EventType::Wheel { delta_x, delta_y } => {
            mark_mouse_ready(core);
            record_scroll(app, core, delta_x, delta_y);
        }
    }
}

fn spawn_listener(app: AppHandle, core: Arc<Core>) {
    if core.listener_started.swap(true, Ordering::SeqCst) {
        return;
    }
    set_input_status(&core, None, Some(InputHealth::Requested), None);
    thread::spawn(move || {
        set_input_status(&core, None, Some(InputHealth::Installing), None);
        let listener_core = core.clone();
        // rdev::listen 没有“安装成功”回调；首次鼠标事件前保持 installing，
        // 这样无用户输入时不会被误报成监听损坏。
        let result = rdev::listen(move |event| handle_input(&app, &listener_core, event));
        core.listener_started.store(false, Ordering::SeqCst);
        match result {
            Err(err) => {
                append_log(&format!("global input listener failed: {err:?}"));
                set_input_status(
                    &core,
                    None,
                    Some(InputHealth::Failed),
                    Some(format!("全局输入监听失败：{err:?}")),
                );
            }
            Ok(()) => {
                append_log("global input listener exited");
                set_input_status(
                    &core,
                    None,
                    Some(InputHealth::Stopped),
                    Some("全局输入监听已退出，请点击重试".into()),
                );
            }
        }
    });
}

#[tauri::command]
fn set_hotkeys(core: State<'_, Arc<Core>>, hotkeys: HashMap<String, String>) -> Result<(), String> {
    let mut map = HashMap::new();
    for (action, combo) in hotkeys {
        if !HOTKEY_ACTIONS.contains(&action.as_str()) {
            continue;
        }
        if combo.trim().is_empty() {
            continue;
        }
        let Some(combo) = normalize_combo(&combo) else {
            return Err(format!("热键格式不正确：{combo}"));
        };
        if let Some(reason) = hotkey_policy_error(&combo) {
            return Err(format!("热键 {combo} 不可用：{reason}"));
        }
        if map.contains_key(&combo) {
            return Err(format!("热键 {combo} 重复绑定"));
        }
        map.insert(combo, action);
    }
    *core.hotkey_map.lock().unwrap_or_else(|e| e.into_inner()) = map;
    Ok(())
}

#[tauri::command]
fn log_frontend_error(message: String) {
    let message = message.chars().take(2_000).collect::<String>();
    append_log(&format!("frontend error: {message}"));
}

#[tauri::command]
fn input_status(core: State<'_, Arc<Core>>) -> InputStatus {
    core.input_status
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone()
}

#[tauri::command]
fn retry_input(app: AppHandle, core: State<'_, Arc<Core>>) -> bool {
    let core = core.inner().clone();
    let (keyboard_state, mouse_state) = {
        let status = core.input_status.lock().unwrap_or_else(|e| e.into_inner());
        (status.keyboard_state, status.mouse_state)
    };
    let mut attempted = false;
    if input_needs_retry(keyboard_state) {
        install_keyboard_hook(core.clone());
        attempted = true;
    }
    if input_needs_retry(mouse_state) {
        spawn_listener(app, core);
        attempted = true;
    }
    attempted
}

#[tauri::command]
fn start_hotkey_capture(core: State<'_, Arc<Core>>) {
    *core.capture_mute.lock().unwrap_or_else(|e| e.into_inner()) = None;
    core.capture.store(true, Ordering::SeqCst);
}

#[tauri::command]
fn cancel_hotkey_capture(core: State<'_, Arc<Core>>, mute: Option<bool>) {
    core.capture.store(false, Ordering::SeqCst);
    let until = if mute.unwrap_or(false) {
        Some(Instant::now())
    } else {
        None
    };
    *core.capture_mute.lock().unwrap_or_else(|e| e.into_inner()) = until;
}

// ---------------------------------------------------------------- 脚本库

#[derive(Serialize, Clone)]
#[serde(rename_all = "camelCase")]
struct ScriptInfo {
    file_name: String,
    name: String,
    count: usize,
    saved: String,
    available: bool,
    reason: Option<String>,
}

#[derive(Clone)]
struct CachedScriptMeta {
    len: u64,
    modified: Option<SystemTime>,
    info: ScriptInfo,
}

struct LoadedScript {
    file: String,
    modified: Option<SystemTime>,
    events: Arc<Vec<ScriptEvent>>,
}

struct ScriptLibrarySnapshot {
    rows: Arc<Vec<ScriptInfo>>,
    created: Instant,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ScriptPage {
    file_name: String,
    total: usize,
    offset: usize,
    events: Vec<ScriptEvent>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ScriptLibraryPage {
    items: Vec<ScriptInfo>,
    total: usize,
    offset: usize,
    limit: usize,
    snapshot: String,
}

#[derive(Serialize, Deserialize, Clone)]
#[serde(rename_all = "camelCase")]
struct TrashEntry {
    id: String,
    file_name: String,
    name: String,
    deleted: String,
}

fn script_path(file: &str) -> Result<PathBuf, String> {
    let p = Path::new(file);
    if p.file_name().map(|n| n.to_string_lossy().to_string()) != Some(file.to_string()) {
        return Err("脚本名称不合法".into());
    }
    if !file.to_lowercase().ends_with(".json") || file.len() <= 5 {
        return Err("脚本名称不合法".into());
    }
    Ok(scripts_dir().join(file))
}

fn parse_script_events(text: &str) -> Result<Vec<ScriptEvent>, String> {
    let doc: Value =
        serde_json::from_str(text).map_err(|_| "脚本加载失败：JSON 格式不正确".to_string())?;
    let events_value = doc
        .get("events")
        .cloned()
        .ok_or_else(|| "脚本加载失败：缺少 events 数组".to_string())?;
    let events: Vec<ScriptEvent> = serde_json::from_value(events_value)
        .map_err(|_| "脚本加载失败：事件结构不正确".to_string())?;
    if events.len() > MAX_EVENTS {
        return Err(format!("脚本加载失败：事件数超过 {MAX_EVENTS}"));
    }
    if !events.iter().all(ScriptEvent::valid) {
        return Err("脚本加载失败：存在超出范围的坐标、间隔或事件参数".into());
    }
    Ok(events)
}

fn inspect_script(
    path: &Path,
    name: &str,
    meta: &fs::Metadata,
    modified: Option<SystemTime>,
) -> ScriptInfo {
    let saved = modified
        .map(|t| {
            let dt: chrono::DateTime<chrono::Local> = t.into();
            dt.format("%Y-%m-%d %H:%M").to_string()
        })
        .unwrap_or_default();
    let unavailable = |reason: String| ScriptInfo {
        file_name: name.to_string(),
        name: name[..name.len() - 5].to_string(),
        count: 0,
        saved: saved.clone(),
        available: false,
        reason: Some(reason),
    };
    if meta.len() > MAX_SCRIPT_BYTES {
        return unavailable(format!(
            "文件过大（上限 {} MB）",
            MAX_SCRIPT_BYTES / 1024 / 1024
        ));
    }
    let text = match fs::read_to_string(path) {
        Ok(text) => text,
        Err(err) => return unavailable(format!("文件无法读取：{err}")),
    };
    match parse_script_events(&text) {
        Ok(events) => ScriptInfo {
            file_name: name.to_string(),
            name: name[..name.len() - 5].to_string(),
            count: events.len(),
            saved,
            available: true,
            reason: None,
        },
        Err(err) => unavailable(err),
    }
}

fn scan_scripts(core: &Core) -> Vec<ScriptInfo> {
    let dir = scripts_dir();
    let mut rows: Vec<(u64, String, ScriptInfo)> = Vec::new();
    let Ok(entries) = fs::read_dir(&dir) else {
        return Vec::new();
    };
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        if name.len() <= 5 || !name.to_lowercase().ends_with(".json") {
            continue;
        }
        let Ok(meta) = entry.metadata() else { continue };
        let modified = meta.modified().ok();
        let path = entry.path();
        let cached = {
            let cache = core.script_meta.lock().unwrap_or_else(|e| e.into_inner());
            cache.get(&path).cloned()
        };
        let info = match cached {
            Some(row) if row.len == meta.len() && row.modified == modified => row.info,
            _ => {
                let info = inspect_script(&path, &name, &meta, modified);
                core.script_meta
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .insert(
                        path.clone(),
                        CachedScriptMeta {
                            len: meta.len(),
                            modified,
                            info: info.clone(),
                        },
                    );
                info
            }
        };
        let order = modified
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);
        rows.push((order, name, info));
    }
    rows.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(&b.1)));
    rows.into_iter().map(|(_, _, row)| row).collect()
}

#[tauri::command]
fn list_scripts(core: State<'_, Arc<Core>>) -> Vec<ScriptInfo> {
    scan_scripts(core.inner())
        .into_iter()
        .take(LIBRARY_PAGE_MAX)
        .collect()
}

#[tauri::command]
fn list_scripts_page(
    core: State<'_, Arc<Core>>,
    offset: usize,
    limit: usize,
    snapshot: Option<String>,
) -> ScriptLibraryPage {
    let cached = snapshot.as_deref().and_then(|token| {
        core.script_pages
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(token)
            .map(|item| (token.to_string(), item.rows.clone()))
    });
    let (snapshot, rows) = match cached {
        Some(value) => value,
        None => {
            let rows = Arc::new(scan_scripts(core.inner()));
            let id = core.next_script_page.fetch_add(1, Ordering::Relaxed) + 1;
            let token = format!("library-{id}");
            let mut pages = core.script_pages.lock().unwrap_or_else(|e| e.into_inner());
            if pages.len() >= 8 {
                if let Some(oldest) = pages
                    .iter()
                    .min_by_key(|(_, item)| item.created)
                    .map(|(key, _)| key.clone())
                {
                    pages.remove(&oldest);
                }
            }
            pages.insert(
                token.clone(),
                ScriptLibrarySnapshot {
                    rows: rows.clone(),
                    created: Instant::now(),
                },
            );
            (token, rows)
        }
    };
    let total = rows.len();
    let offset = offset.min(total);
    let limit = limit.clamp(1, LIBRARY_PAGE_MAX);
    let items = rows.iter().skip(offset).take(limit).cloned().collect();
    ScriptLibraryPage {
        items,
        total,
        offset,
        limit,
        snapshot,
    }
}

fn load_script_arc(core: &Arc<Core>, file: &str) -> Result<Arc<Vec<ScriptEvent>>, String> {
    core.ensure_no_pending_recording("加载脚本")?;
    let fail = || "脚本加载失败：文件格式不正确".to_string();
    let path = script_path(file).map_err(|_| fail())?;
    let meta = fs::metadata(&path).map_err(|_| fail())?;
    if meta.len() > MAX_SCRIPT_BYTES {
        return Err("脚本加载失败：文件过大".into());
    }
    let modified = meta.modified().ok();
    if let Some(loaded) = core
        .loaded_script
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .as_ref()
        .filter(|loaded| loaded.file == file && loaded.modified == modified)
    {
        return Ok(loaded.events.clone());
    }
    let text = fs::read_to_string(&path).map_err(|_| fail())?;
    let events = Arc::new(parse_script_events(&text)?);
    *core.loaded_script.lock().unwrap_or_else(|e| e.into_inner()) = Some(LoadedScript {
        file: file.to_string(),
        modified,
        events: events.clone(),
    });
    Ok(events)
}

#[tauri::command]
fn load_script(core: State<'_, Arc<Core>>, file: String) -> Result<Vec<ScriptEvent>, String> {
    Ok((*load_script_arc(core.inner(), &file)?).clone())
}

#[tauri::command]
fn load_script_page(
    core: State<'_, Arc<Core>>,
    file: String,
    offset: usize,
    limit: usize,
) -> Result<ScriptPage, String> {
    let events = load_script_arc(core.inner(), &file)?;
    let offset = offset.min(events.len());
    let limit = limit.clamp(1, SCRIPT_PAGE_MAX);
    let page = events
        .iter()
        .skip(offset)
        .take(limit)
        .cloned()
        .collect::<Vec<_>>();
    Ok(ScriptPage {
        file_name: file,
        total: events.len(),
        offset,
        events: page,
    })
}

#[tauri::command]
fn start_playback_file(
    app: AppHandle,
    core: State<'_, Arc<Core>>,
    file: String,
    speed: f64,
    loops: u64,
) -> Result<u64, String> {
    let events = load_script_arc(core.inner(), &file)?;
    spawn_playback(app, core.inner().clone(), events, speed, loops)
}

fn trash_root() -> PathBuf {
    base_dir().join("trash").join("scripts")
}

static TRASH_SEQ: AtomicU64 = AtomicU64::new(0);

fn move_script_to_trash_in(
    path: &Path,
    trash: &Path,
    file_name: &str,
    name: &str,
) -> Result<TrashEntry, String> {
    fs::create_dir_all(trash).map_err(|_| "删除失败：无法创建回收站".to_string())?;
    let seq = TRASH_SEQ.fetch_add(1, Ordering::Relaxed) + 1;
    let id = format!(
        "{}-{}-{}",
        chrono::Local::now().format("%Y%m%d-%H%M%S-%3f"),
        std::process::id(),
        seq
    );
    let item_dir = trash.join(&id);
    fs::create_dir(&item_dir).map_err(|_| "删除失败：无法创建回收站条目".to_string())?;
    let stored = item_dir.join("script.json");
    fs::rename(path, &stored).map_err(|_| "删除失败：文件被占用或不存在".to_string())?;
    let entry = TrashEntry {
        id,
        file_name: file_name.to_string(),
        name: name.to_string(),
        deleted: chrono::Local::now().format("%Y-%m-%d %H:%M").to_string(),
    };
    let manifest =
        serde_json::to_string_pretty(&entry).map_err(|_| "删除失败：无法写入回收站".to_string())?;
    if let Err(err) = write_atomic(&item_dir.join("meta.json"), &manifest) {
        let _ = fs::rename(&stored, path);
        let _ = fs::remove_dir(&item_dir);
        return Err(format!("删除失败：无法写入回收站（{err}）"));
    }
    Ok(entry)
}

fn restore_script_in(trash: &Path, scripts: &Path, id: &str) -> Result<String, String> {
    if Path::new(id)
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        != Some(id.to_string())
    {
        return Err("恢复失败：回收站条目无效".into());
    }
    let item_dir = trash.join(id);
    let manifest = fs::read_to_string(item_dir.join("meta.json"))
        .map_err(|_| "恢复失败：回收站条目缺失".to_string())?;
    let entry: TrashEntry =
        serde_json::from_str(&manifest).map_err(|_| "恢复失败：回收站条目损坏".to_string())?;
    let source = item_dir.join("script.json");
    if !source.exists() {
        return Err("恢复失败：脚本文件缺失".into());
    }
    fs::create_dir_all(scripts).map_err(|_| "恢复失败：无法创建脚本目录".to_string())?;
    let original = script_path(&entry.file_name)?;
    let stem = entry.file_name.trim_end_matches(".json");
    let mut target = scripts.join(&entry.file_name);
    let mut n = 1;
    while target.exists() {
        target = scripts.join(format!("{stem}-恢复-{n}.json"));
        n += 1;
    }
    fs::rename(&source, &target).map_err(|_| "恢复失败：文件被占用".to_string())?;
    let _ = fs::remove_file(item_dir.join("meta.json"));
    let _ = fs::remove_dir(&item_dir);
    let _ = original;
    target
        .file_name()
        .map(|name| name.to_string_lossy().to_string())
        .ok_or_else(|| "恢复失败：目标名称无效".to_string())
}

#[tauri::command]
fn delete_script(core: State<'_, Arc<Core>>, file: String) -> Result<TrashEntry, String> {
    let path = script_path(&file).map_err(|_| "删除失败：文件被占用或不存在".to_string())?;
    let name = file.trim_end_matches(".json").to_string();
    let entry = move_script_to_trash_in(&path, &trash_root(), &file, &name)?;
    core.script_meta
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .remove(&path);
    let mut loaded = core.loaded_script.lock().unwrap_or_else(|e| e.into_inner());
    if loaded.as_ref().is_some_and(|item| item.file == file) {
        *loaded = None;
    }
    Ok(entry)
}

#[tauri::command]
fn list_trash() -> Vec<TrashEntry> {
    let root = trash_root();
    let Ok(entries) = fs::read_dir(root) else {
        return Vec::new();
    };
    let mut rows = entries
        .flatten()
        .filter_map(|entry| {
            let text = fs::read_to_string(entry.path().join("meta.json")).ok()?;
            serde_json::from_str::<TrashEntry>(&text).ok()
        })
        .collect::<Vec<_>>();
    rows.sort_by(|a, b| b.deleted.cmp(&a.deleted));
    rows
}

#[tauri::command]
fn restore_script(core: State<'_, Arc<Core>>, id: String) -> Result<String, String> {
    let restored = restore_script_in(&trash_root(), &scripts_dir(), &id)?;
    core.script_meta
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clear();
    Ok(restored)
}

#[tauri::command]
fn rename_script(
    core: State<'_, Arc<Core>>,
    file: String,
    new_name: String,
) -> Result<String, String> {
    let old_path = script_path(&file).map_err(|_| "重命名失败：文件被占用或不存在".to_string())?;
    let new_name = new_name.trim().to_string();
    if new_name.is_empty() {
        return Err("重命名失败：名称为空".into());
    }
    if new_name.chars().any(|c| SCRIPT_BAD.contains(c)) {
        return Err("重命名失败：名称含非法字符 < > : \" / \\ | ? *".into());
    }
    if new_name.chars().count() > 80 {
        return Err("重命名失败：名称过长（80 字以内）".into());
    }
    if new_name.ends_with('.') {
        return Err("重命名失败：名称不能以点结尾".into());
    }
    if is_reserved_name(&new_name) {
        return Err("重命名失败：名称是 Windows 保留设备名".into());
    }
    let target = scripts_dir().join(format!("{new_name}.json"));
    if target.exists() {
        return Err(format!("重命名失败：「{new_name}」已存在"));
    }
    fs::rename(&old_path, &target).map_err(|_| "重命名失败：文件被占用或不存在".to_string())?;
    core.script_meta
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .remove(&old_path);
    *core.loaded_script.lock().unwrap_or_else(|e| e.into_inner()) = None;
    Ok(format!("{new_name}.json"))
}

#[tauri::command]
fn open_scripts_folder() -> Result<(), String> {
    let dir = scripts_dir();
    fs::create_dir_all(&dir).map_err(|_| "无法创建脚本文件夹".to_string())?;
    std::process::Command::new("explorer")
        .arg(&dir)
        .spawn()
        .map_err(|_| "无法打开脚本文件夹".to_string())?;
    Ok(())
}

// ---------------------------------------------------------------- 设置 / 系统

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct LoadResult {
    doc: Option<Value>,
    warning: Option<String>,
}

fn settings_backup_path(path: &Path) -> PathBuf {
    let preferred = path.with_extension("json.bak");
    if !preferred.exists() {
        return preferred;
    }
    let stamp = chrono::Local::now().format("%Y%m%d-%H%M%S-%3f");
    path.with_file_name(format!("settings-{stamp}.json.bak"))
}

fn prune_settings_backups(path: &Path) {
    let Some(parent) = path.parent() else { return };
    let Ok(entries) = fs::read_dir(parent) else {
        return;
    };
    let mut backups = entries
        .flatten()
        .filter(|entry| {
            entry
                .file_name()
                .to_string_lossy()
                .to_lowercase()
                .starts_with("settings")
                && entry
                    .file_name()
                    .to_string_lossy()
                    .to_lowercase()
                    .ends_with(".json.bak")
        })
        .filter_map(|entry| {
            let modified = entry.metadata().ok()?.modified().ok()?;
            Some((modified, entry.path()))
        })
        .collect::<Vec<_>>();
    backups.sort_by_key(|(modified, _)| std::cmp::Reverse(*modified));
    for (_, old) in backups.into_iter().skip(3) {
        let _ = fs::remove_file(old);
    }
}

#[tauri::command]
fn load_settings() -> LoadResult {
    let path = settings_path();
    let meta = match fs::metadata(&path) {
        Ok(meta) => meta,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            return LoadResult {
                doc: None,
                warning: None,
            }
        }
        Err(err) => {
            append_log(&format!("settings metadata failed: {err}"));
            return LoadResult {
                doc: None,
                warning: Some("设置文件无法读取，本次使用默认值".into()),
            };
        }
    };
    if meta.len() > MAX_SETTINGS_BYTES {
        append_log(&format!("settings file too large: {} bytes", meta.len()));
        return LoadResult {
            doc: None,
            warning: Some("设置文件过大，已忽略并使用默认值".into()),
        };
    }
    let text = match fs::read_to_string(&path) {
        Ok(text) => text,
        Err(err) => {
            append_log(&format!("settings read failed: {err}"));
            return LoadResult {
                doc: None,
                warning: Some("设置文件无法读取，本次使用默认值".into()),
            };
        }
    };
    match serde_json::from_str::<Value>(&text) {
        Ok(doc) => LoadResult {
            doc: Some(doc),
            warning: None,
        },
        Err(err) => {
            let backup = settings_backup_path(&path);
            let moved = fs::rename(&path, &backup).is_ok();
            append_log(&format!(
                "settings parse failed: {err}; backup={} moved={moved}",
                backup.display()
            ));
            LoadResult {
                doc: None,
                warning: Some(if moved {
                    "设置文件损坏，已备份并使用默认值".into()
                } else {
                    "设置文件损坏且无法备份，本次使用默认值".into()
                }),
            }
        }
    }
}

static WRITE_SEQ: AtomicU64 = AtomicU64::new(0);

fn write_atomic(path: &Path, text: &str) -> std::io::Result<()> {
    let seq = WRITE_SEQ.fetch_add(1, Ordering::Relaxed) + 1;
    let filename = path
        .file_name()
        .map(|name| name.to_string_lossy())
        .unwrap_or_default();
    let tmp = path.with_file_name(format!(".{filename}.{}.{}.tmp", std::process::id(), seq));
    let result = (|| {
        fs::write(&tmp, text)?;
        if let Ok(file) = fs::OpenOptions::new().write(true).read(true).open(&tmp) {
            file.sync_all()?;
        }
        match fs::rename(&tmp, path) {
            Ok(()) => Ok(()),
            Err(_first) if path.exists() => {
                let backup = settings_backup_path(path);
                fs::rename(path, &backup)?;
                match fs::rename(&tmp, path) {
                    Ok(()) => {
                        prune_settings_backups(path);
                        Ok(())
                    }
                    Err(err) => {
                        let _ = fs::rename(&backup, path);
                        Err(err)
                    }
                }
            }
            Err(err) => Err(err),
        }
    })();
    let _ = fs::remove_file(&tmp);
    result
}

fn setting_i64(value: &Value, field: &str) -> Result<Option<i64>, String> {
    if value.is_null() {
        return Ok(None);
    }
    let text = value
        .as_str()
        .map(str::to_string)
        .or_else(|| value.as_i64().map(|n| n.to_string()));
    let Some(text) = text else {
        return Err(format!("设置字段 {field} 不是整数"));
    };
    let text = text.trim();
    if text.is_empty() {
        return Ok(None);
    }
    text.parse::<i64>()
        .map(Some)
        .map_err(|_| format!("设置字段 {field} 不是整数"))
}

fn validate_settings_doc(settings: &Value) -> Result<(), String> {
    if let Some(ui) = settings.get("ui").and_then(Value::as_object) {
        if let Some(value) = ui.get("interval") {
            if let Some(n) = setting_i64(value, "interval")? {
                if !(10..=MAX_INTERVAL_MS as i64).contains(&n) {
                    return Err(format!("点击间隔需在 10–{MAX_INTERVAL_MS} 毫秒之间"));
                }
            }
        }
        let finite_count = ui.get("cnt_mode").and_then(Value::as_u64) == Some(1);
        if finite_count {
            if let Some(value) = ui.get("count") {
                if let Some(n) = setting_i64(value, "count")? {
                    if !(1..=MAX_CLICKS as i64).contains(&n) {
                        return Err(format!("点击次数需在 1–{MAX_CLICKS} 之间"));
                    }
                }
            }
        }
        if let Some(value) = ui.get("loops") {
            if let Some(n) = setting_i64(value, "loops")? {
                if !(0..=MAX_LOOPS as i64).contains(&n) {
                    return Err(format!("回放循环次数需在 0–{MAX_LOOPS} 之间"));
                }
            }
        }
        for field in ["pos_x", "pos_y"] {
            if let Some(value) = ui.get(field) {
                if let Some(n) = setting_i64(value, field)? {
                    if !(-100_000..=100_000).contains(&n) {
                        return Err(format!("坐标 {field} 需在 -100000–100000 之间"));
                    }
                }
            }
        }
    }
    if let Some(hotkeys) = settings.get("hotkeys").and_then(Value::as_object) {
        for action in HOTKEY_ACTIONS {
            if let Some(value) = hotkeys.get(action) {
                let raw = value
                    .as_str()
                    .ok_or_else(|| format!("热键 {action} 必须是字符串"))?;
                if raw.trim().is_empty() {
                    continue;
                }
                let combo = normalize_combo(raw).ok_or_else(|| format!("热键格式不正确：{raw}"))?;
                if let Some(reason) = hotkey_policy_error(&combo) {
                    return Err(format!("热键 {combo} 不可用：{reason}"));
                }
            }
        }
    }
    if let Some(button) = settings.get("hold_button").and_then(Value::as_str) {
        if !matches!(button, "left" | "middle" | "right") {
            return Err("长按按键设置无效".into());
        }
    }
    Ok(())
}

#[tauri::command]
fn save_settings(core: State<'_, Arc<Core>>, settings: Value) -> Result<(), String> {
    if !settings.is_object() {
        return Err("设置必须是 JSON 对象".into());
    }
    validate_settings_doc(&settings)?;
    let text = serde_json::to_string_pretty(&settings).map_err(|e| e.to_string())?;
    if text.len() as u64 > MAX_SETTINGS_BYTES {
        return Err("设置内容过大".into());
    }
    let _guard = core
        .settings_write
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    write_atomic(&settings_path(), &text).map_err(|e| e.to_string())
}

#[tauri::command]
fn get_cursor() -> Result<[i32; 2], String> {
    let (x, y) = try_cursor_pos()?;
    Ok([x, y])
}

#[tauri::command]
fn set_topmost(window: tauri::WebviewWindow, enabled: bool) -> Result<(), String> {
    window.set_always_on_top(enabled).map_err(|e| e.to_string())
}

#[tauri::command]
fn is_admin() -> bool {
    unsafe {
        let mut token: HANDLE = ptr::null_mut();
        if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) == 0 {
            return false;
        }
        let mut elevation: TOKEN_ELEVATION = std::mem::zeroed();
        let mut size: DWORD = 0;
        let ok = GetTokenInformation(
            token,
            TokenElevation,
            &mut elevation as *mut _ as LPVOID,
            std::mem::size_of::<TOKEN_ELEVATION>() as DWORD,
            &mut size,
        );
        CloseHandle(token);
        ok != 0 && elevation.TokenIsElevated != 0
    }
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

fn begin_close_request(app: &AppHandle, core: &Arc<Core>) {
    if !core.closing.swap(true, Ordering::SeqCst) {
        let _ = app.emit("close-requested", json!({}));
    }
}

#[tauri::command]
fn request_close(app: AppHandle, core: State<'_, Arc<Core>>) {
    begin_close_request(&app, core.inner());
}

#[tauri::command]
fn prepare_close(app: AppHandle, core: State<'_, Arc<Core>>) -> Result<ClosePreparation, String> {
    let core = core.inner().clone();
    core.closing.store(true, Ordering::SeqCst);
    if core.recording.load(Ordering::SeqCst) {
        let rect = main_window_rect(&app);
        stop_recording_internal(&core, rect)?;
    }
    if pending_recording_info(&core).is_some() {
        let _ = try_save_pending(&core);
    }
    core.request_stop_all();
    let stopped = core.wait_idle(TASK_STOP_TIMEOUT);
    let pending = pending_recording_info(&core);
    let error = if stopped {
        None
    } else {
        Some("仍有任务未完成输入释放，已取消退出".to_string())
    };
    let ready = pending.is_none() && error.is_none();
    core.close_ready.store(ready, Ordering::SeqCst);
    Ok(ClosePreparation {
        ready,
        pending_recording: pending,
        error,
    })
}

#[tauri::command]
fn cancel_close(core: State<'_, Arc<Core>>) {
    core.closing.store(false, Ordering::SeqCst);
    core.close_ready.store(false, Ordering::SeqCst);
}

#[tauri::command]
fn finish_close(app: AppHandle, core: State<'_, Arc<Core>>) -> Result<(), String> {
    if pending_recording_info(core.inner()).is_some() {
        return Err("仍有未保存的录制，已阻止退出".into());
    }
    if !core.close_ready.load(Ordering::SeqCst) {
        return Err("退出准备尚未完成".into());
    }
    if !core
        .task
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .is_none()
    {
        return Err("仍有任务未完成，已阻止退出".into());
    }
    app.exit(0);
    Ok(())
}

#[tauri::command]
fn elevate(app: AppHandle, core: State<'_, Arc<Core>>) -> Result<(), String> {
    if !core.close_ready.load(Ordering::SeqCst) {
        return Err("提权前必须先完成保存和任务停止".into());
    }
    let exe = std::env::current_exe().map_err(|_| "无法获取程序路径".to_string())?;
    let ready_name = format!(
        "Local\\AutoClicker_HandoffReady_{}_{}",
        std::process::id(),
        WRITE_SEQ.fetch_add(1, Ordering::Relaxed) + 1
    );
    let event_name = wide(&ready_name);
    let ready_event = unsafe { CreateEventW(ptr::null_mut(), 1, 0, event_name.as_ptr()) };
    if ready_event.is_null() {
        return Err("无法创建提权交接事件".into());
    }
    let verb = wide("runas");
    let file = wide(&exe.to_string_lossy());
    let params = wide(&format!("--elevated-handoff --handoff-ready={ready_name}"));
    release_instance_handle();
    let ret = unsafe {
        ShellExecuteW(
            ptr::null_mut(),
            verb.as_ptr() as LPCWSTR,
            file.as_ptr() as LPCWSTR,
            params.as_ptr() as LPCWSTR,
            ptr::null(),
            SW_SHOWNORMAL,
        )
    };
    if ret as isize > 32 {
        // UAC 已同意且子进程已创建后，旧实例才释放单例；新实例初始化完成后发 ready 信号。
        release_instance_handle();
        let wait = unsafe { WaitForSingleObject(ready_event, HANDOFF_READY_TIMEOUT_MS) };
        unsafe {
            CloseHandle(ready_event);
        }
        if wait == 0 {
            app.exit(0);
            return Ok(());
        }
        let reacquired = reacquire_instance_handle();
        return Err(if reacquired {
            "新管理员实例未在时限内完成初始化，旧实例已继续运行".into()
        } else {
            "新管理员实例未确认就绪，旧实例保留运行；请检查是否已有其他实例".into()
        });
    }
    let code = std::io::Error::last_os_error().raw_os_error().unwrap_or(0);
    unsafe {
        CloseHandle(ready_event);
    }
    // ShellExecute 失败/UAC 取消时旧实例仍持有原单例句柄。
    if code == 1223 {
        Err("已取消获取管理员权限".into())
    } else {
        Err(format!("无法以管理员身份启动（错误码 {code}）"))
    }
}

fn main() {
    if !single_instance_guard() {
        return;
    }
    append_log("application starting");
    tauri::Builder::default()
        .manage(Arc::new(Core::new()))
        .setup(|app| {
            if std::env::var_os("AUTOCLICKER_BACKGROUND_TEST").is_some() {
                if let Some(window) = app.get_webview_window("main") {
                    let _ = window.hide();
                }
                return Ok(());
            }
            let core = app.state::<Arc<Core>>().inner().clone();
            spawn_keyboard_hook(app.handle().clone(), core.clone());
            spawn_listener(app.handle().clone(), core);
            signal_handoff_ready();
            Ok(())
        })
        .on_window_event(|window, event| {
            if let tauri::WindowEvent::CloseRequested { api, .. } = event {
                api.prevent_close();
                let app = window.app_handle();
                let core = app.state::<Arc<Core>>().inner().clone();
                begin_close_request(app, &core);
            }
        })
        .invoke_handler(tauri::generate_handler![
            start_clicker,
            stop_clicker,
            start_hold,
            stop_hold,
            start_playback,
            start_playback_file,
            stop_playback,
            task_status,
            start_recording,
            stop_recording,
            set_hotkeys,
            start_hotkey_capture,
            cancel_hotkey_capture,
            list_scripts,
            list_scripts_page,
            load_script,
            load_script_page,
            delete_script,
            list_trash,
            restore_script,
            rename_script,
            open_scripts_folder,
            retry_pending_recording,
            save_pending_recording_as,
            discard_pending_recording_command,
            load_settings,
            save_settings,
            get_cursor,
            set_topmost,
            is_admin,
            input_status,
            retry_input,
            log_frontend_error,
            request_close,
            prepare_close,
            cancel_close,
            finish_close,
            elevate,
            beep
        ])
        .run(tauri::generate_context!())
        .unwrap_or_else(|err| {
            append_log(&format!("application run failed: {err}"));
        });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn vk_names_match_hotkey_labels() {
        assert_eq!(vk_name(0x75), Some("f6"));
        assert_eq!(vk_name(0x41), Some("a"));
        assert_eq!(vk_name(0x31), Some("1"));
        assert_eq!(vk_name(0xDE), Some("'"));
        assert_eq!(vk_name(0xA1), None);
        assert_eq!(vk_modifier(0xA2), Some("ctrl"));
        assert_eq!(vk_modifier(0x12), Some("alt"));
        assert_eq!(vk_modifier(0x5B), Some("cmd"));
    }

    #[test]
    fn combos_keep_modifier_order_and_main_key() {
        let mods = HashSet::from(["shift".to_string(), "ctrl".to_string()]);
        assert_eq!(combo_string(&mods, "f6"), "ctrl+shift+f6");
        let mods = HashSet::from(["cmd".to_string(), "alt".to_string()]);
        assert_eq!(combo_string(&mods, "a"), "alt+cmd+a");
        assert_eq!(combo_string(&HashSet::new(), "esc"), "esc");
    }

    fn ev(etype: &str, pressed: Option<bool>) -> ScriptEvent {
        ScriptEvent {
            ev_type: etype.into(),
            dt: 0,
            x: 1,
            y: 2,
            button: Some("left".into()),
            pressed,
            dx: None,
            dy: None,
        }
    }

    #[test]
    fn script_event_validation() {
        assert!(ev("click", Some(true)).valid());
        assert!(!ev("click", None).valid());
        assert!(ev("move", None).valid());
        let mut scroll = ev("scroll", None);
        scroll.dx = Some(1);
        scroll.dy = Some(-1);
        assert!(scroll.valid());
        scroll.dy = None;
        assert!(!scroll.valid());
    }

    #[test]
    fn trim_tail_removes_stop_click() {
        let mut events = vec![
            ScriptEvent {
                ev_type: "click".into(),
                dt: 0,
                x: 100,
                y: 100,
                button: Some("left".into()),
                pressed: Some(true),
                dx: None,
                dy: None,
            },
            ScriptEvent {
                ev_type: "click".into(),
                dt: 10,
                x: 500,
                y: 300,
                button: Some("left".into()),
                pressed: Some(true),
                dx: None,
                dy: None,
            },
            ScriptEvent {
                ev_type: "click".into(),
                dt: 20,
                x: 500,
                y: 300,
                button: Some("left".into()),
                pressed: Some(false),
                dx: None,
                dy: None,
            },
        ];
        trim_tail(&mut events, (400, 200, 900, 600));
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].x, 100);
    }

    #[test]
    fn script_path_rejects_traversal() {
        assert!(script_path("../evil.json").is_err());
        assert!(script_path("sub/evil.json").is_err());
        assert!(script_path("evil.txt").is_err());
        assert!(script_path("good.json").is_ok());
    }

    #[test]
    #[ignore = "性能冒烟：用 run-perf.cmd（--release --ignored）运行"]
    fn validating_hundred_thousand_events_stays_fast() {
        let events: Vec<ScriptEvent> = (0..100_000)
            .map(|i| ScriptEvent {
                ev_type: "move".into(),
                dt: i % 1000,
                x: i % 90_000,
                y: (i * 3) % 90_000,
                button: None,
                pressed: None,
                dx: None,
                dy: None,
            })
            .collect();
        let started = Instant::now();
        assert!(events.iter().all(|event| event.valid()));
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "10 万条事件的边界校验耗时过长：{:?}",
            started.elapsed()
        );
    }

    #[test]
    fn playback_validation_rejects_empty_events() {
        let bad = ScriptEvent {
            ev_type: "click".into(),
            dt: 0,
            x: 0,
            y: 0,
            button: None,
            pressed: None,
            dx: None,
            dy: None,
        };
        assert!(!bad.valid());
    }

    #[test]
    fn combo_normalization_orders_and_rejects_bad_input() {
        assert_eq!(
            normalize_combo("Shift+Ctrl+F6").as_deref(),
            Some("ctrl+shift+f6")
        );
        assert_eq!(normalize_combo("ctrl+ctrl+a"), None);
        assert_eq!(normalize_combo("a+b"), None);
        assert_eq!(normalize_combo("ctrl+"), None);
        assert_eq!(normalize_combo("hyper+a"), None);
        assert_eq!(normalize_combo("ctrl+1").as_deref(), Some("ctrl+1"));
    }

    #[test]
    fn reserved_device_names_are_rejected() {
        assert!(is_reserved_name("CON"));
        assert!(is_reserved_name("con.json"));
        assert!(is_reserved_name("COM1"));
        assert!(is_reserved_name("lpt9"));
        assert!(!is_reserved_name("COM0"));
        assert!(!is_reserved_name("CONSOLE"));
        assert!(!is_reserved_name("录制 1"));
    }

    #[test]
    fn event_bounds_reject_out_of_range_values() {
        let mut ev = ScriptEvent {
            ev_type: "move".into(),
            dt: 0,
            x: 0,
            y: 0,
            button: None,
            pressed: None,
            dx: None,
            dy: None,
        };
        assert!(ev.valid());
        ev.x = 100_001;
        assert!(!ev.valid());
        ev.x = 0;
        ev.dt = -1;
        assert!(!ev.valid());
        ev.dt = 3_600_001;
        assert!(!ev.valid());
    }

    #[test]
    fn scroll_events_reject_overflowing_wheel_steps() {
        let ev = ScriptEvent {
            ev_type: "scroll".into(),
            dt: 0,
            x: 0,
            y: 0,
            button: None,
            pressed: None,
            dx: Some(1_001),
            dy: Some(0),
        };
        assert!(!ev.valid());
    }
    #[test]
    fn point_in_rect_handles_edges() {
        let rect = (10, 20, 30, 40);
        assert!(point_in_rect(10, 20, rect));
        assert!(point_in_rect(29, 39, rect));
        assert!(!point_in_rect(30, 39, rect));
        assert!(!point_in_rect(29, 40, rect));
    }

    #[test]
    fn input_health_distinguishes_installing_failure_and_ready() {
        let core = Arc::new(Core::new());
        set_input_status(&core, Some(InputHealth::Requested), None, None);
        let status = core
            .input_status
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        assert_eq!(status.keyboard_state, InputHealth::Requested);
        assert!(!status.keyboard);
        assert!(!input_needs_retry(InputHealth::Requested));
        set_input_status(&core, Some(InputHealth::Installing), None, None);
        assert!(!input_needs_retry(InputHealth::Installing));
        set_input_status(
            &core,
            Some(InputHealth::Failed),
            None,
            Some("failed".into()),
        );
        let status = core
            .input_status
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        assert_eq!(status.keyboard_state, InputHealth::Failed);
        assert!(input_needs_retry(status.keyboard_state));
        set_input_status(&core, Some(InputHealth::Ready), None, None);
        assert!(!input_needs_retry(InputHealth::Ready));
        mark_mouse_ready(&core);
        let status = core
            .input_status
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        assert_eq!(status.keyboard_state, InputHealth::Ready);
        assert_eq!(status.mouse_state, InputHealth::Ready);
        assert!(status.keyboard && status.mouse);
    }

    #[test]
    fn task_slot_stays_reserved_until_worker_completion() {
        let core = Arc::new(Core::new());
        let (id, _stop, done) = core.reserve(TaskKind::Play).unwrap();
        assert!(core.reserve(TaskKind::Play).is_err());
        let worker_core = core.clone();
        let worker_done = done.clone();
        let worker = thread::spawn(move || {
            thread::sleep(Duration::from_millis(40));
            worker_core.complete(id);
            let _ = worker_done;
        });
        assert!(core.wait_idle(Duration::from_secs(2)));
        worker.join().unwrap();
        assert!(!core.snapshot().running);
        assert!(core.reserve(TaskKind::Clicker).is_ok());
    }

    #[test]
    fn old_hold_owner_cannot_release_new_generation() {
        let mut state = Some(HoldState {
            id: 2,
            btn: Btn::Right,
        });
        assert_eq!(take_hold_for_id(&mut state, 1), None);
        assert!(state.is_some());
        assert_eq!(take_hold_for_id(&mut state, 2), Some(Btn::Right));
        assert!(state.is_none());
    }

    #[test]
    fn panic_corner_is_bounded_and_supports_negative_monitors() {
        let primary = (0, 0, 1920, 1080);
        assert!(panic_corner_hit(0, 0, primary));
        assert!(panic_corner_hit(2, 2, primary));
        assert!(!panic_corner_hit(3, 2, primary));
        assert!(!panic_corner_hit(-100, -100, primary));
        let left = (-1920, 0, 0, 1080);
        assert!(panic_corner_hit(-1920, 0, left));
        assert!(!panic_corner_hit(-100, 0, left));
    }

    #[test]
    fn monitor_rectangles_reject_gaps_and_keep_negative_coordinates() {
        let left = (-1920, 0, 0, 1080);
        let right = (0, 0, 1920, 1080);
        assert!(point_in_rect_i32(-10, 10, left));
        assert!(!point_in_rect_i32(0, 10, left));
        assert!(point_in_rect_i32(0, 10, right));
        assert!(!point_in_rect_i32(-1, 10, right));
    }

    #[test]
    fn long_recording_delays_are_split_without_shortening_total_time() {
        let event = ScriptEvent {
            ev_type: "click".into(),
            dt: MAX_EVENT_DELAY_MS + 17,
            x: 10,
            y: 20,
            button: Some("left".into()),
            pressed: Some(true),
            dx: None,
            dy: None,
        };
        let parts = split_long_delay(event);
        assert_eq!(parts.len(), 2);
        assert_eq!(parts[0].ev_type, "wait");
        assert_eq!(parts[0].dt, MAX_EVENT_DELAY_MS);
        assert_eq!(parts[1].dt, 17);
        assert_eq!(
            parts.iter().map(|part| part.dt).sum::<i64>(),
            MAX_EVENT_DELAY_MS + 17
        );
        assert!(parts.iter().all(ScriptEvent::valid));
    }

    #[test]
    fn script_validation_reports_bad_contract_and_accepts_wait() {
        let wait = ev("wait", None);
        assert!(wait.valid());
        let bad = r#"{"events":[{"type":"click","dt":0,"x":0,"y":0}]}"#;
        assert!(parse_script_events(bad).is_err());
        let too_long = r#"{"events":[{"type":"move","dt":3600001,"x":0,"y":0}]}"#;
        assert!(parse_script_events(too_long).is_err());
        let good = r#"{"events":[{"type":"wait","dt":3600000,"x":0,"y":0}]}"#;
        assert_eq!(parse_script_events(good).unwrap().len(), 1);
    }

    #[test]
    fn settings_validation_matches_runtime_limits_and_hotkey_policy() {
        let valid = json!({
            "hotkeys": {"clicker":"ctrl+;","record":"f8","play":"f9","hold":"f7"},
            "hold_button": "left",
            "ui": {"interval":"10","count":"1","loops":"1000000","pos_x":"-100000","pos_y":"100000"}
        });
        assert!(validate_settings_doc(&valid).is_ok());
        let bad_interval = json!({"ui":{"interval":"9"}});
        assert!(validate_settings_doc(&bad_interval).is_err());
        let bad_loop = json!({"ui":{"loops":"1000001"}});
        assert!(validate_settings_doc(&bad_loop).is_err());
        let infinite_zero = json!({"ui":{"cnt_mode":0,"count":"0"}});
        assert!(validate_settings_doc(&infinite_zero).is_ok());
        let finite_zero = json!({"ui":{"cnt_mode":1,"count":"0"}});
        assert!(validate_settings_doc(&finite_zero).is_err());
        let unsafe_key = json!({"hotkeys":{"clicker":"a"}});
        assert!(validate_settings_doc(&unsafe_key).is_err());
        let symbol = normalize_combo("ctrl+;");
        assert_eq!(symbol.as_deref(), Some("ctrl+;"));
        assert!(hotkey_policy_error("ctrl+;").is_none());
        assert!(hotkey_policy_error("alt+f4").is_some());
    }

    #[test]
    fn trash_restore_does_not_overwrite_new_same_name_file() {
        let base = std::env::var_os("AUTOCLICKER_TEST_ROOT")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(std::env::temp_dir)
            .join(format!(
                "autoclicker-trash-test-{}-{}",
                std::process::id(),
                TRASH_SEQ.load(Ordering::Relaxed)
            ));
        let scripts = base.join("scripts");
        let trash = base.join("trash");
        fs::create_dir_all(&scripts).unwrap();
        let source = scripts.join("same.json");
        fs::write(&source, "original").unwrap();
        let entry = move_script_to_trash_in(&source, &trash, "same.json", "same").unwrap();
        assert!(!source.exists());
        fs::write(&source, "new").unwrap();
        let restored = restore_script_in(&trash, &scripts, &entry.id).unwrap();
        assert_eq!(restored, "same-恢复-1.json");
        assert_eq!(fs::read_to_string(&source).unwrap(), "new");
        assert_eq!(
            fs::read_to_string(scripts.join(&restored)).unwrap(),
            "original"
        );
        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn inspect_script_marks_bad_file_visible_instead_of_dropping_it() {
        let base = std::env::var_os("AUTOCLICKER_TEST_ROOT")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(std::env::temp_dir)
            .join(format!(
                "autoclicker-inspect-test-{}-{}",
                std::process::id(),
                WRITE_SEQ.load(Ordering::Relaxed)
            ));
        fs::create_dir_all(&base).unwrap();
        let path = base.join("bad.json");
        fs::write(&path, "{bad").unwrap();
        let meta = fs::metadata(&path).unwrap();
        let info = inspect_script(&path, "bad.json", &meta, meta.modified().ok());
        assert!(!info.available);
        assert!(info.reason.is_some());
        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn recording_save_failure_keeps_full_pending_snapshot() {
        let core = Arc::new(Core::new());
        core.recording.store(true, Ordering::SeqCst);
        core.rec_events
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(ev("click", Some(true)));
        let result = stop_recording_with_saver(&core, None, |_| Err("disk denied".into())).unwrap();
        assert_eq!(result.events.len(), 1);
        assert!(result.pending_save);
        assert_eq!(result.save_error.as_deref(), Some("disk denied"));
        let pending = pending_recording_info(&core).unwrap();
        assert_eq!(pending.event_count, 1);
        assert_eq!(pending.error, "disk denied");
        assert!(core
            .pending_recording
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
            .is_some_and(|item| item.events.len() == 1));
    }

    #[test]
    fn script_atomic_write_does_not_touch_settings_backups() {
        let base = std::env::var_os("AUTOCLICKER_TEST_ROOT")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(std::env::temp_dir)
            .join(format!(
                "autoclicker-script-atomic-test-{}-{}",
                std::process::id(),
                WRITE_SEQ.load(Ordering::Relaxed)
            ));
        fs::create_dir_all(&base).unwrap();
        let path = base.join("script.json");
        write_atomic_script(&path, r#"{"events":[]}"#).unwrap();
        write_atomic_script(&path, r#"{"events":[1]}"#).unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), r#"{"events":[1]}"#);
        let leftovers = fs::read_dir(&base)
            .unwrap()
            .flatten()
            .filter(|entry| {
                let name = entry.file_name().to_string_lossy().to_string();
                name.contains(".tmp")
                    || name.contains(".script.bak")
                    || name.starts_with("settings-")
            })
            .count();
        assert_eq!(leftovers, 0);
        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn write_atomic_replaces_without_leaving_temp_files() {
        let base = std::env::var_os("AUTOCLICKER_TEST_ROOT")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(std::env::temp_dir);
        let dir = base.join(format!(
            "autoclicker-atomic-test-{}-{}",
            std::process::id(),
            WRITE_SEQ.load(Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("settings.json");
        write_atomic(&path, "{\"version\":1}").unwrap();
        write_atomic(&path, "{\"version\":2}").unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "{\"version\":2}");
        let leftovers = std::fs::read_dir(&dir)
            .unwrap()
            .flatten()
            .filter(|entry| entry.file_name().to_string_lossy().contains(".tmp"))
            .count();
        assert_eq!(leftovers, 0);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
