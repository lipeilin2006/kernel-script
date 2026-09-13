use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use mlua::{Function, IntoLuaMulti, Lua, VmState};

static CLOCK_START: OnceLock<Instant> = OnceLock::new();

pub fn time_us() -> u64 {
    CLOCK_START.get_or_init(Instant::now).elapsed().as_micros() as u64
}

pub fn install_hook(lua: &Lua) -> std::sync::Arc<AtomicU64> {
    let deadline = std::sync::Arc::new(AtomicU64::new(0));
    let hook_deadline = std::sync::Arc::clone(&deadline);
    lua.set_interrupt(move |_| {
        let deadline_us = hook_deadline.load(Ordering::Relaxed);
        if deadline_us != 0 && time_us() >= deadline_us {
            return Err(mlua::Error::runtime("script execution budget exceeded"));
        }
        Ok(VmState::Continue)
    });
    deadline
}

pub fn call_budgeted<A>(
    lua: &Lua,
    deadline: &AtomicU64,
    name: &str,
    args: A,
    budget: Duration,
) -> mlua::Result<()>
where
    A: IntoLuaMulti,
{
    let callback = lua.globals().get::<Option<Function>>(name)?;
    let Some(callback) = callback else {
        return Ok(());
    };

    deadline.store(deadline_us(budget), Ordering::Relaxed);
    let result = callback.call::<()>(args);
    deadline.store(0, Ordering::Relaxed);
    result
}

pub fn set_deadline(deadline: &AtomicU64, budget: Option<Duration>) {
    let value = budget.map_or(0, deadline_us);
    deadline.store(value, Ordering::Relaxed);
}

fn deadline_us(budget: Duration) -> u64 {
    time_us().saturating_add(budget.as_micros() as u64)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn runaway_script_is_interrupted() {
        let lua = Lua::new();
        let deadline = install_hook(&lua);
        lua.load("function OnUpdate() while true do end end")
            .exec()
            .unwrap();
        let error =
            call_budgeted(&lua, &deadline, "OnUpdate", (), Duration::from_millis(1)).unwrap_err();
        assert!(error.to_string().contains("execution budget exceeded"));
    }
}
