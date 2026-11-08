//! Lua keyboard input API.
//!
//! Keys are polled with `GetAsyncKeyState`, which reads the physical key
//! state of the whole desktop — queries work while another window (the
//! game) owns input focus. The engine snapshots all 256 virtual keys once
//! per frame (`begin_frame`); queries within a frame read that snapshot, so
//! detection granularity is one GUI frame (100 Hz). `is_key_press` latches a
//! full press-then-release cycle and is consumed when read.

use std::sync::{Arc, Mutex};

use mlua::{Lua, Value};
use windows_sys::Win32::UI::Input::KeyboardAndMouse::GetAsyncKeyState;

pub type SharedKeyboardState = Arc<Mutex<KeyboardState>>;

pub struct KeyboardState {
    prev: [bool; 256],
    curr: [bool; 256],
    /// Set when a full down→up cycle is observed; cleared by is_key_press.
    latch: [bool; 256],
}

impl KeyboardState {
    pub fn new() -> Self {
        Self {
            prev: [false; 256],
            curr: [false; 256],
            latch: [false; 256],
        }
    }

    /// Rolls the current snapshot into the previous one, re-reads the live
    /// key state, and latches completed press cycles (down last frame, up
    /// this frame). Called once per GUI frame before OnUpdate.
    pub fn begin_frame(&mut self) {
        self.prev = self.curr;
        for (vk, slot) in self.curr.iter_mut().enumerate() {
            *slot = key_down(vk);
            if self.prev[vk] && !*slot {
                self.latch[vk] = true;
            }
        }
    }

    pub fn is_down(&self, vk: usize) -> bool {
        self.curr[vk]
    }

    pub fn is_up(&self, vk: usize) -> bool {
        !self.curr[vk]
    }

    /// Returns and clears the press-cycle latch: true when the key was
    /// pressed and released since the last call.
    pub fn take_press(&mut self, vk: usize) -> bool {
        std::mem::take(&mut self.latch[vk])
    }
}

impl Default for KeyboardState {
    fn default() -> Self {
        Self::new()
    }
}

fn key_down(vk: usize) -> bool {
    unsafe { GetAsyncKeyState(vk as i32) as u16 & 0x8000 != 0 }
}

/// Resolves a Lua key argument to a virtual-key code. Accepts a VK code
/// (integer 0–255) or one of the recognized key names.
fn resolve_key(key: &Value) -> mlua::Result<usize> {
    match key {
        Value::Integer(code) => {
            if (0..=255).contains(code) {
                Ok(*code as usize)
            } else {
                Err(mlua::Error::runtime(format!(
                    "key code out of range 0-255: {code}"
                )))
            }
        }
        Value::String(name) => {
            let name = name.to_str()?;
            vk_from_name(&name)
                .ok_or_else(|| mlua::Error::runtime(format!("unknown key name: {name}")))
        }
        other => Err(mlua::Error::runtime(format!(
            "key must be a name or VK code, got {other:?}"
        ))),
    }
}

fn vk_from_name(name: &str) -> Option<usize> {
    let key = name.trim().to_ascii_lowercase();
    let vk = match key.as_str() {
        "lbutton" | "mouse1" => 0x01,
        "rbutton" | "mouse2" => 0x02,
        "cancel" => 0x03,
        "mbutton" | "mouse3" => 0x04,
        "xbutton1" | "mouse4" => 0x05,
        "xbutton2" | "mouse5" => 0x06,
        "back" | "backspace" => 0x08,
        "tab" => 0x09,
        "return" | "enter" => 0x0D,
        "shift" => 0x10,
        "lshift" => 0xA0,
        "rshift" => 0xA1,
        "control" | "ctrl" => 0x11,
        "lctrl" | "lcontrol" => 0xA2,
        "rctrl" | "rcontrol" => 0xA3,
        "alt" | "menu" => 0x12,
        "lalt" | "lmenu" => 0xA4,
        "ralt" | "rmenu" => 0xA5,
        "pause" => 0x13,
        "capslock" | "capital" => 0x14,
        "escape" | "esc" => 0x1B,
        "space" => 0x20,
        "pageup" | "pgup" => 0x21,
        "pagedown" | "pgdn" => 0x22,
        "end" => 0x23,
        "home" => 0x24,
        "left" => 0x25,
        "up" => 0x26,
        "right" => 0x27,
        "down" => 0x28,
        "insert" | "ins" => 0x2D,
        "delete" | "del" => 0x2E,
        "lwin" => 0x5B,
        "rwin" => 0x5C,
        "apps" => 0x5D,
        "numpad0" => 0x60,
        "numpad1" => 0x61,
        "numpad2" => 0x62,
        "numpad3" => 0x63,
        "numpad4" => 0x64,
        "numpad5" => 0x65,
        "numpad6" => 0x66,
        "numpad7" => 0x67,
        "numpad8" => 0x68,
        "numpad9" => 0x69,
        "multiply" => 0x6A,
        "add" => 0x6B,
        "separator" => 0x6C,
        "subtract" => 0x6D,
        "decimal" => 0x6E,
        "divide" => 0x6F,
        _ => return vk_from_fallback(&key),
    };
    Some(vk)
}

fn vk_from_fallback(key: &str) -> Option<usize> {
    let mut chars = key.chars();
    let (first, rest) = (chars.next()?, chars.collect::<String>());
    if rest.is_empty() {
        if first.is_ascii_lowercase() {
            return Some(0x41 + (first as usize - 'a' as usize));
        }
        if first.is_ascii_uppercase() {
            return Some(0x41 + (first as usize - 'A' as usize));
        }
        if first.is_ascii_digit() {
            return Some(0x30 + (first as usize - '0' as usize));
        }
        return None;
    }
    if (first == 'f' || first == 'F') && !rest.is_empty() {
        if let Ok(number) = rest.parse::<usize>() {
            if (1..=24).contains(&number) {
                return Some(0x70 + number - 1);
            }
        }
    }
    None
}

fn shared_state(state: &SharedKeyboardState) -> std::sync::MutexGuard<'_, KeyboardState> {
    state.lock().expect("keyboard state poisoned")
}

pub fn register(lua: &Lua, state: SharedKeyboardState) -> mlua::Result<()> {
    let module = lua.create_table()?;

    // keyboard.is_key_down(key) -> bool
    // True while the key is held. `key` is a name ("a", "f1", "insert",
    // "shift", "mouse1", ...) or a raw VK code (integer 0-255).
    {
        let state = Arc::clone(&state);
        module.set(
            "is_key_down",
            lua.create_function(move |_, key: Value| {
                let vk = resolve_key(&key)?;
                Ok(shared_state(&state).is_down(vk))
            })?,
        )?;
    }

    // keyboard.is_key_up(key) -> bool
    {
        let state = Arc::clone(&state);
        module.set(
            "is_key_up",
            lua.create_function(move |_, key: Value| {
                let vk = resolve_key(&key)?;
                Ok(shared_state(&state).is_up(vk))
            })?,
        )?;
    }

    // keyboard.is_key_press(key) -> bool
    // True when the key was pressed and released since the last call (a
    // full press cycle). The flag latches across frames and is consumed by
    // this call, so a single press fires exactly once.
    {
        let state = Arc::clone(&state);
        module.set(
            "is_key_press",
            lua.create_function(move |_, key: Value| {
                let vk = resolve_key(&key)?;
                Ok(shared_state(&state).take_press(vk))
            })?,
        )?;
    }

    lua.globals().set("keyboard", module)
}
