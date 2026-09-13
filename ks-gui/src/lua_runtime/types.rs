use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use mlua::{FromLua, Lua, Value};

#[derive(Clone, Debug)]
pub enum DrawCommand {
    Line {
        x1: f32,
        y1: f32,
        x2: f32,
        y2: f32,
        color: [u8; 4],
        thickness: f32,
    },
    Rect {
        x: f32,
        y: f32,
        w: f32,
        h: f32,
        color: [u8; 4],
        thickness: f32,
    },
    FilledRect {
        x: f32,
        y: f32,
        w: f32,
        h: f32,
        color: [u8; 4],
    },
    Circle {
        x: f32,
        y: f32,
        radius: f32,
        color: [u8; 4],
        thickness: f32,
    },
    FilledCircle {
        x: f32,
        y: f32,
        radius: f32,
        color: [u8; 4],
    },
    Text {
        x: f32,
        y: f32,
        text: String,
        color: [u8; 4],
        size: f32,
    },
}

pub type DrawCommands = Arc<Mutex<Vec<DrawCommand>>>;

#[derive(Clone, Copy, Debug)]
pub struct Address(u64);

impl Address {
    pub fn new(value: u64) -> Self {
        Self(value)
    }

    pub fn get(self) -> u64 {
        self.0
    }
}

impl FromLua for Address {
    fn from_lua(value: Value, _lua: &Lua) -> mlua::Result<Self> {
        match value {
            Value::Integer(value) if value > 0 => Ok(Self(value as u64)),
            Value::Integer(_) => Err(mlua::Error::runtime("address must not be negative")),
            Value::String(value) => super::parse_address(value.to_str()?.as_ref()),
            value => Err(mlua::Error::runtime(format!(
                "address must be a positive integer or string, got {}",
                value.type_name()
            ))),
        }
    }
}

#[derive(Default)]
pub struct RuntimeControl {
    pub paused: AtomicBool,
    pub pending_steps: AtomicUsize,
}

impl RuntimeControl {
    pub fn consume_manual_step(&self) -> usize {
        self.pending_steps.swap(0, Ordering::Relaxed).min(1)
    }
}
