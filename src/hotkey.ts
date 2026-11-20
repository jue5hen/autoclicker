const MODIFIER_NAMES = ["ctrl", "alt", "shift", "cmd"] as const;
type Modifier = (typeof MODIFIER_NAMES)[number];

const MODIFIER_KEYS = new Set([
  "Alt",
  "AltGraph",
  "CapsLock",
  "Control",
  "Fn",
  "FnLock",
  "Hyper",
  "Meta",
  "NumLock",
  "ScrollLock",
  "Shift",
  "Super",
  "Symbol",
  "SymbolLock",
]);

const CODE_NAMES: Record<string, string> = {
  Backquote: "`",
  Minus: "-",
  Equal: "=",
  BracketLeft: "[",
  BracketRight: "]",
  Backslash: "\\",
  Semicolon: ";",
  Quote: "'",
  Comma: ",",
  Period: ".",
  Slash: "/",
  Space: "space",
  Escape: "esc",
  Enter: "enter",
  Tab: "tab",
  Backspace: "backspace",
  Delete: "delete",
  Home: "home",
  End: "end",
  PageUp: "pageup",
  PageDown: "pagedown",
  ArrowLeft: "left",
  ArrowUp: "up",
  ArrowRight: "right",
  ArrowDown: "down",
};

const NAMED_MAIN_KEYS = new Set([
  "space",
  "esc",
  "enter",
  "tab",
  "backspace",
  "delete",
  "home",
  "end",
  "pageup",
  "pagedown",
  "left",
  "up",
  "right",
  "down",
]);

const SYMBOL_MAIN_KEYS = new Set([
  "`",
  "-",
  "=",
  "[",
  "]",
  "\\",
  ";",
  "'",
  ",",
  ".",
  "/",
]);

const SYSTEM_RESERVED = new Set([
  "alt+f4",
  "alt+tab",
  "alt+esc",
  "ctrl+esc",
  "ctrl+shift+esc",
  "ctrl+alt+delete",
]);

const COMBO_LABELS: Record<string, string> = {
  ctrl: "Ctrl",
  alt: "Alt",
  shift: "Shift",
  cmd: "Win",
  esc: "Esc",
  space: "Space",
  pageup: "PageUp",
  pagedown: "PageDown",
  up: "↑",
  down: "↓",
  left: "←",
  right: "→",
};

export type HotkeyValidation =
  | { ok: true; combo: string }
  | { ok: false; reason: string };

export function isModifierKey(event: KeyboardEvent): boolean {
  return MODIFIER_KEYS.has(event.key);
}

const KEY_NAMES: Record<string, string> = {
  " ": "space",
  Escape: "esc",
  Enter: "enter",
  Tab: "tab",
  Backspace: "backspace",
  Delete: "delete",
  Home: "home",
  End: "end",
  PageUp: "pageup",
  PageDown: "pagedown",
  ArrowLeft: "left",
  ArrowUp: "up",
  ArrowRight: "right",
  ArrowDown: "down",
};

function isSupportedMainKey(name: string) {
  return (
    /^[a-z0-9]$/.test(name) ||
    NAMED_MAIN_KEYS.has(name) ||
    SYMBOL_MAIN_KEYS.has(name) ||
    /^f(?:[1-9]|1[0-2])$/.test(name)
  );
}

export function keyNameFromEvent(event: KeyboardEvent): string | null {
  const code = event.code || "";
  if (/^Key[A-Z]$/.test(code)) return code.slice(3).toLowerCase();
  if (/^Digit[0-9]$/.test(code)) return code.slice(5);
  if (/^F([1-9]|1[0-2])$/.test(code)) return code.toLowerCase();
  if (CODE_NAMES[code]) return CODE_NAMES[code];
  // 部分输入方式（例如合成按键）不提供 code，只能退回 key。
  if (/^F([1-9]|1[0-2])$/i.test(event.key)) return event.key.toLowerCase();
  const named = KEY_NAMES[event.key];
  if (named) return named;
  if (event.key.length === 1) {
    const lower = event.key.toLowerCase();
    if (isSupportedMainKey(lower)) return lower;
  }
  return null;
}

export function hotkeyFromEvent(event: KeyboardEvent): string | null {
  if (isModifierKey(event)) return null;
  const name = keyNameFromEvent(event);
  if (!name) return null;
  const mods: string[] = [];
  if (event.ctrlKey) mods.push("ctrl");
  if (event.altKey) mods.push("alt");
  if (event.shiftKey) mods.push("shift");
  if (event.metaKey) mods.push("cmd");
  return [...mods, name].join("+");
}

export function comboText(combo: string) {
  return combo
    .split("+")
    .map((part) => {
      if (!part) return part;
      const lower = part.toLowerCase();
      if (COMBO_LABELS[lower]) return COMBO_LABELS[lower];
      if (lower.length === 1) return lower.toUpperCase();
      if (/^f\d+$/.test(lower)) return lower.toUpperCase();
      return part[0].toUpperCase() + part.slice(1);
    })
    .join("+");
}

export function normalizeCombo(text: unknown): string | null {
  const parts = String(text ?? "")
    .toLowerCase()
    .split("+")
    .map((part) => part.trim());
  if (!parts.length || parts.some((part) => !part)) return null;

  const mods: Modifier[] = [];
  let main: string | null = null;
  for (const part of parts) {
    if ((MODIFIER_NAMES as readonly string[]).includes(part)) {
      if (mods.includes(part as Modifier)) return null;
      mods.push(part as Modifier);
      continue;
    }
    if (!isSupportedMainKey(part) || main !== null) return null;
    main = part;
  }
  if (main === null) return null;
  return [...MODIFIER_NAMES.filter((mod) => mods.includes(mod)), main].join("+");
}

/**
 * 前后端共享的安全策略。裸字母/数字/符号很容易在打字时误触；
 * Win 组合键和系统保留组合键由 Windows 优先处理，应用不应假装能可靠接管。
 */
export function validateHotkey(combo: unknown): HotkeyValidation {
  const normalized = normalizeCombo(combo);
  if (!normalized) {
    return { ok: false, reason: "热键格式无效" };
  }
  const parts = normalized.split("+");
  const main = parts[parts.length - 1];
  const mods = parts.slice(0, -1);
  if (!mods.length && !/^f(?:[1-9]|1[0-2])$/.test(main)) {
    return { ok: false, reason: "裸键容易误触，请至少添加 Ctrl、Alt 或 Shift" };
  }
  if (mods.includes("cmd")) {
    return { ok: false, reason: "不支持 Win 组合键，系统可能优先截获" };
  }
  if (SYSTEM_RESERVED.has(normalized)) {
    return { ok: false, reason: "该组合键属于 Windows 系统保留动作，不能可靠用作全局热键" };
  }
  return { ok: true, combo: normalized };
}
